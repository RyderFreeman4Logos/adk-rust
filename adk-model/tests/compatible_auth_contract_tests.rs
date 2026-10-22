#![cfg(feature = "openai")]

use adk_core::{AdkError, Content, ErrorCategory, Llm, LlmRequest, LlmResponse};
use adk_model::openai::{AzureConfig, AzureOpenAIClient, OpenAIClient, OpenAIConfig};
use adk_model::retry::RetryConfig;
use adk_model::{OpenAICompatible, OpenAICompatibleConfig};
use futures::TryStreamExt;
use serde_json::json;
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use wiremock::{Mock, MockServer, ResponseTemplate, matchers::method};

fn config(server: &MockServer, key: &str) -> OpenAICompatibleConfig {
    OpenAICompatibleConfig::new(key, "synthetic-model").with_base_url(server.uri())
}

fn response(stream: bool) -> ResponseTemplate {
    if stream {
        ResponseTemplate::new(200).insert_header("content-type", "text/event-stream").set_body_string(
            "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"ok\"},\"finish_reason\":null}]}\n\n\
             data: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n\
             data: [DONE]\n\n",
        )
    } else {
        ResponseTemplate::new(200).set_body_json(json!({
            "id":"synthetic", "model":"synthetic-model", "created":0,
            "choices":[{"index":0,"message":{"role":"assistant","content":"ok"},"finish_reason":"stop"}]
        }))
    }
}

async fn collect(model: &dyn Llm, stream: bool) -> Result<Vec<LlmResponse>, AdkError> {
    let request = LlmRequest::new("synthetic-model", vec![Content::new("user").with_text("hello")]);
    model.generate_content(request, stream).await?.try_collect().await
}

#[tokio::test]
async fn authenticated_blank_credentials_fail_before_egress() {
    for stream in [false, true] {
        for key in ["", " ", "\t", "\n", "synthetic-private\nvalue"] {
            let server = MockServer::start().await;
            Mock::given(method("POST")).respond_with(response(stream)).mount(&server).await;
            let result = match OpenAICompatible::new(config(&server, key)) {
                Ok(model) => {
                    collect(&model.with_retry_config(RetryConfig::disabled()), stream).await
                }
                Err(error) => Err(error),
            };
            assert!(result.is_err(), "authenticated blank credential was admitted");
            let error = result.unwrap_err();
            assert_eq!(error.category, ErrorCategory::InvalidInput);
            assert_eq!(error.code, "model.openai_compat.invalid_api_key");
            assert!(!error.to_string().contains("synthetic-private"));
            assert!(server.received_requests().await.unwrap().is_empty());
        }
    }
}

fn no_auth(server: &MockServer) -> OpenAICompatible {
    // Existing serialized configuration remains compatible; no key field or lookup.
    let config = serde_json::from_value(json!({
        "provider_name":"synthetic-local", "model":"synthetic-model",
        "base_url":server.uri(), "parallel_tool_calls":true
    }))
    .unwrap();
    OpenAICompatible::new_no_auth(config).unwrap()
}

#[tokio::test]
async fn explicit_no_auth_preserves_requests_and_ignores_unused_key() {
    for stream in [false, true] {
        for ignored_key in [false, true] {
            let server = MockServer::start().await;
            Mock::given(method("POST"))
                .respond_with(response(stream))
                .expect(1)
                .mount(&server)
                .await;
            let model = if ignored_key {
                OpenAICompatible::new_no_auth(config(&server, "synthetic-ignored\ncredential"))
                    .unwrap()
            } else {
                no_auth(&server)
            };
            let mut headers = http::HeaderMap::new();
            headers.insert("x-local", "default".parse().unwrap());
            let model = model.with_default_headers(headers).unwrap().with_request_adapter(
                Arc::new(|body, headers| {
                    body["metadata"] = json!({"fixture":"no-auth"});
                    headers.insert("x-adapter", "kept".parse().unwrap());
                    Ok(())
                }),
            );
            let output = collect(&model, stream).await.unwrap();
            assert!(output.iter().any(|item| item.turn_complete));
            let requests = server.received_requests().await.unwrap();
            assert_eq!(requests.len(), 1);
            let request = &requests[0];
            assert!(request.headers.get("authorization").is_none());
            assert_eq!(request.headers.get("x-local").unwrap(), "default");
            assert_eq!(request.headers.get("x-adapter").unwrap(), "kept");
            let body: serde_json::Value = serde_json::from_slice(&request.body).unwrap();
            assert_eq!(body["metadata"], json!({"fixture":"no-auth"}));
            assert_eq!(
                body.get("stream").and_then(|value| value.as_bool()).unwrap_or(false),
                stream
            );
        }
    }
}

fn assert_forbidden(error: &AdkError) {
    assert_eq!(error.category, ErrorCategory::InvalidInput);
    assert_eq!(error.code, "model.openai_compat.authorization_forbidden");
    assert!(!error.to_string().contains("synthetic-injected"));
    assert!(!error.retry.should_retry);
}

#[tokio::test]
async fn no_auth_rejects_default_authorization_in_either_builder_order() {
    for name in ["authorization", "Authorization", "AUTHORIZATION"] {
        for adapter_first in [false, true] {
            let server = MockServer::start().await;
            let model = no_auth(&server);
            let adapter = Arc::new(|_: &mut serde_json::Value, _: &mut http::HeaderMap| Ok(()));
            let mut headers = http::HeaderMap::new();
            headers.insert(
                http::HeaderName::from_bytes(name.as_bytes()).unwrap(),
                "Bearer synthetic-injected".parse().unwrap(),
            );
            let result = if adapter_first {
                model.with_request_adapter(adapter).with_default_headers(headers)
            } else {
                model.with_default_headers(headers).map(|model| model.with_request_adapter(adapter))
            };
            let error = result.err().expect("NoAuth default Authorization must fail closed");
            assert_forbidden(&error);
            assert!(server.received_requests().await.unwrap().is_empty());
        }
    }
}

fn fast_retry() -> RetryConfig {
    RetryConfig::default()
        .with_max_retries(1)
        .with_initial_delay(std::time::Duration::ZERO)
        .with_max_delay(std::time::Duration::ZERO)
}

#[tokio::test]
async fn no_auth_checks_adapter_on_each_attempt_without_retrying_policy_errors() {
    for stream in [false, true] {
        for inject_attempt in [0, 1] {
            let server = MockServer::start().await;
            Mock::given(method("POST"))
                .respond_with(ResponseTemplate::new(503))
                .expect(inject_attempt as u64)
                .mount(&server)
                .await;
            let attempts = Arc::new(AtomicUsize::new(0));
            let observed = attempts.clone();
            let model = no_auth(&server).with_retry_config(fast_retry()).with_request_adapter(
                Arc::new(move |_, headers| {
                    if observed.fetch_add(1, Ordering::SeqCst) == inject_attempt {
                        headers.insert(
                            http::HeaderName::from_bytes(b"AuThOrIzAtIoN").unwrap(),
                            "Bearer synthetic-injected".parse().unwrap(),
                        );
                    }
                    Ok(())
                }),
            );
            assert_forbidden(&collect(&model, stream).await.unwrap_err());
            assert_eq!(attempts.load(Ordering::SeqCst), inject_attempt + 1);
            let requests = server.received_requests().await.unwrap();
            assert_eq!(requests.len(), inject_attempt);
            assert!(requests.iter().all(|request| request.headers.get("authorization").is_none()));
        }
    }
}

#[tokio::test]
async fn no_auth_retries_keep_header_absent_and_unauthorized_does_not_retry() {
    for stream in [false, true] {
        for status in [503, 401] {
            let server = MockServer::start().await;
            let calls = AtomicUsize::new(0);
            Mock::given(method("POST"))
                .respond_with(move |_: &wiremock::Request| {
                    if calls.fetch_add(1, Ordering::SeqCst) == 0 {
                        ResponseTemplate::new(status)
                    } else {
                        response(stream)
                    }
                })
                .expect(if status == 503 { 2 } else { 1 })
                .mount(&server)
                .await;
            let result = collect(&no_auth(&server).with_retry_config(fast_retry()), stream).await;
            if status == 503 {
                assert!(result.unwrap().iter().any(|item| item.turn_complete));
            } else {
                assert_eq!(result.unwrap_err().category, ErrorCategory::Unauthorized);
            }
            let requests = server.received_requests().await.unwrap();
            assert_eq!(requests.len(), if status == 503 { 2 } else { 1 });
            assert!(requests.iter().all(|request| request.headers.get("authorization").is_none()));
        }
    }
}

#[tokio::test]
async fn authenticated_adapters_retain_delegated_bearer_behavior() {
    for stream in [false, true] {
        for delegated in [false, true] {
            let server = MockServer::start().await;
            Mock::given(method("POST"))
                .respond_with(response(stream))
                .expect(1)
                .mount(&server)
                .await;
            let mut headers = http::HeaderMap::new();
            headers
                .insert(http::header::AUTHORIZATION, "Bearer synthetic-default".parse().unwrap());
            let mut model = OpenAICompatible::new(config(&server, "synthetic-configured"))
                .unwrap()
                .with_default_headers(headers)
                .unwrap();
            if delegated {
                model = model.with_request_adapter(Arc::new(|_, headers| {
                    headers.insert(
                        http::header::AUTHORIZATION,
                        "Bearer synthetic-delegated".parse().unwrap(),
                    );
                    Ok(())
                }));
            }
            assert!(collect(&model, stream).await.unwrap().iter().any(|item| item.turn_complete));
            let requests = server.received_requests().await.unwrap();
            assert_eq!(requests.len(), 1);
            assert_eq!(
                requests[0].headers.get("authorization").unwrap(),
                if delegated {
                    "Bearer synthetic-delegated"
                } else {
                    "Bearer synthetic-configured"
                }
            );
        }
    }
}

#[tokio::test]
async fn authenticated_alternate_constructor_and_deserialized_missing_key_reject() {
    let server = MockServer::start().await;
    let missing: OpenAICompatibleConfig = serde_json::from_value(json!({
        "provider_name":"synthetic", "model":"synthetic-model", "base_url":server.uri(), "parallel_tool_calls":true
    })).unwrap();
    assert!(OpenAICompatible::new(missing).is_err());
    assert!(OpenAICompatible::new_with_reasoning_effort(config(&server, " "), None).is_err());
    assert!(OpenAIClient::compatible("", server.uri(), "synthetic-model").is_err());
    assert!(server.received_requests().await.unwrap().is_empty());
}

fn assert_userinfo_rejected(error: &AdkError, endpoint: &str) {
    assert_eq!(error.category, ErrorCategory::InvalidInput);
    assert_eq!(error.code, "model.openai_compat.url_userinfo_forbidden");
    assert_eq!(error.message, "compatible endpoint URLs must not contain userinfo");
    assert!(!error.retry.should_retry);
    assert!(!error.to_string().contains(endpoint));
    assert!(!error.to_string().contains("synthetic-user"));
    assert!(!error.to_string().contains("synthetic-password"));
    assert!(!error.to_string().contains("synthetic-configured"));
}

const USERINFO: [&str; 7] = [
    "synthetic-user",
    "synthetic-user:synthetic-password",
    ":synthetic-password",
    "synthetic-user:",
    "synthetic%2Duser:synthetic%40password",
    "%FF:synthetic-password",
    "%00",
];

#[tokio::test]
async fn compatible_url_userinfo_is_rejected_before_egress() {
    for stream in [false, true] {
        for userinfo in USERINFO {
            for anonymous in [false, true] {
                let server = MockServer::start().await;
                Mock::given(method("POST")).respond_with(response(stream)).mount(&server).await;
                let endpoint = server.uri().replacen("http://", &format!("http://{userinfo}@"), 1);
                let parsed = reqwest::Url::parse(&endpoint).unwrap();
                assert!(!parsed.username().is_empty() || parsed.password().is_some());
                let config =
                    config(&server, "synthetic-configured").with_base_url(endpoint.clone());
                let model = if anonymous {
                    OpenAICompatible::new_no_auth(config)
                } else {
                    OpenAICompatible::new(config)
                }
                .unwrap()
                .with_retry_config(fast_retry());
                let adapter_calls = Arc::new(AtomicUsize::new(0));
                let observed_calls = adapter_calls.clone();
                let model = model.with_request_adapter(Arc::new(move |_, _| {
                    observed_calls.fetch_add(1, Ordering::SeqCst);
                    Ok(())
                }));
                let result = collect(&model, stream).await;
                assert!(result.is_err(), "URL userinfo must fail before sending");
                assert_userinfo_rejected(&result.unwrap_err(), &endpoint);
                assert_eq!(adapter_calls.load(Ordering::SeqCst), 0);
                assert!(server.received_requests().await.unwrap().is_empty());
            }
        }
    }
}

#[tokio::test]
async fn azure_url_userinfo_is_rejected_before_egress() {
    for stream in [false, true] {
        for userinfo in USERINFO {
            let server = MockServer::start().await;
            Mock::given(method("POST")).respond_with(response(stream)).mount(&server).await;
            let endpoint = server.uri().replacen("http://", &format!("http://{userinfo}@"), 1);
            let model = AzureOpenAIClient::new(AzureConfig::new(
                "synthetic-configured",
                endpoint.clone(),
                "2024-12-01-preview",
                "deployment",
            ))
            .unwrap()
            .with_retry_config(fast_retry());
            let result = collect(&model, stream).await;
            assert!(result.is_err(), "Azure completion URL userinfo must fail before sending");
            assert_userinfo_rejected(&result.unwrap_err(), &endpoint);
            assert!(server.received_requests().await.unwrap().is_empty());
        }
    }
}

#[tokio::test]
async fn empty_url_userinfo_retains_existing_authentication() {
    for stream in [false, true] {
        for userinfo in ["", ":"] {
            for mode in ["bearer", "anonymous", "azure"] {
                let server = MockServer::start().await;
                Mock::given(method("POST"))
                    .respond_with(response(stream))
                    .expect(1)
                    .mount(&server)
                    .await;
                let endpoint = server.uri().replacen("http://", &format!("http://{userinfo}@"), 1);
                let parsed = reqwest::Url::parse(&endpoint).unwrap();
                assert!(parsed.username().is_empty());
                assert!(parsed.password().is_none());
                let config =
                    config(&server, "synthetic-configured").with_base_url(endpoint.clone());
                let model: Box<dyn Llm> = match mode {
                    "bearer" => Box::new(OpenAICompatible::new(config).unwrap()),
                    "anonymous" => Box::new(OpenAICompatible::new_no_auth(config).unwrap()),
                    _ => Box::new(
                        AzureOpenAIClient::new(AzureConfig::new(
                            "synthetic-configured",
                            endpoint,
                            "2024-12-01-preview",
                            "deployment",
                        ))
                        .unwrap(),
                    ),
                };
                assert!(
                    collect(model.as_ref(), stream)
                        .await
                        .unwrap()
                        .iter()
                        .any(|item| item.turn_complete)
                );
                let requests = server.received_requests().await.unwrap();
                assert_eq!(requests.len(), 1);
                match mode {
                    "bearer" => assert_eq!(
                        requests[0].headers.get("authorization").unwrap(),
                        "Bearer synthetic-configured"
                    ),
                    "anonymous" => assert!(requests[0].headers.get("authorization").is_none()),
                    _ => {
                        assert!(requests[0].headers.get("authorization").is_none());
                        assert_eq!(
                            requests[0].headers.get("api-key").unwrap(),
                            "synthetic-configured"
                        );
                    }
                }
            }
        }
    }
}

#[cfg(feature = "opencode")]
#[tokio::test]
async fn opencode_chat_keeps_bearer_and_conversation_headers() {
    use adk_model::opencode::{OpenCodeApi, OpenCodeClient, OpenCodeConfig, OpenCodeService};
    for stream in [false, true] {
        let server = MockServer::start().await;
        Mock::given(method("POST")).respond_with(response(stream)).expect(1).mount(&server).await;
        let model = OpenCodeClient::new(
            OpenCodeConfig::new(OpenCodeService::Go, "synthetic-opencode", "synthetic-model")
                .with_api(OpenCodeApi::ChatCompletions)
                .with_base_url(format!("{}/v1", server.uri()))
                .with_user_agent("synthetic-client/1.0")
                .with_session_id("synthetic-conversation")
                .with_retry_config(RetryConfig::disabled()),
        )
        .unwrap();
        assert!(collect(&model, stream).await.unwrap().iter().any(|item| item.turn_complete));
        let requests = server.received_requests().await.unwrap();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].headers.get("authorization").unwrap(), "Bearer synthetic-opencode");
        assert_eq!(
            requests[0].headers.get("x-opencode-session").unwrap(),
            "synthetic-conversation"
        );
        assert_eq!(requests[0].headers.get("user-agent").unwrap(), "synthetic-client/1.0");
    }
}

#[tokio::test]
async fn valid_openai_and_azure_authentication_survives_both_modes() {
    for stream in [false, true] {
        for azure in [false, true] {
            let server = MockServer::start().await;
            Mock::given(method("POST"))
                .respond_with(response(stream))
                .expect(1)
                .mount(&server)
                .await;
            let model: Box<dyn Llm> = if azure {
                Box::new(
                    AzureOpenAIClient::new(AzureConfig::new(
                        "synthetic-azure-key",
                        server.uri(),
                        "2024-12-01-preview",
                        "deployment",
                    ))
                    .unwrap()
                    .with_retry_config(RetryConfig::disabled()),
                )
            } else {
                let mut config = OpenAIConfig::new("synthetic-bearer", "synthetic-model");
                config.base_url = Some(server.uri());
                Box::new(
                    OpenAIClient::new(config).unwrap().with_retry_config(RetryConfig::disabled()),
                )
            };
            let output = collect(model.as_ref(), stream).await.unwrap();
            assert!(output.iter().any(|item| item.turn_complete));
            let requests = server.received_requests().await.unwrap();
            assert_eq!(requests.len(), 1);
            if azure {
                assert!(requests[0].headers.get("authorization").is_none());
                assert_eq!(requests[0].headers.get("api-key").unwrap(), "synthetic-azure-key");
                assert_eq!(
                    requests[0].url.path(),
                    "/openai/deployments/deployment/chat/completions"
                );
                assert_eq!(requests[0].url.query(), Some("api-version=2024-12-01-preview"));
            } else {
                assert_eq!(
                    requests[0].headers.get("authorization").unwrap(),
                    "Bearer synthetic-bearer"
                );
            }
        }
    }
}
