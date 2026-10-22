//! Response-schema contracts at the actual OpenAI-compatible HTTP boundary.

#![cfg(feature = "openai")]

use adk_core::{AdkError, Content, ErrorCategory, Llm, LlmRequest};
use adk_model::{OpenAICompatible, OpenAICompatibleConfig, RetryConfig};
use futures::StreamExt;
use serde_json::{Value, json};
use std::sync::Arc;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn client(server: &MockServer) -> OpenAICompatible {
    OpenAICompatible::new(
        OpenAICompatibleConfig::new("synthetic-key", "schema-model").with_base_url(server.uri()),
    )
    .expect("client")
    .with_retry_config(RetryConfig::disabled())
}

fn request(schema: Value) -> LlmRequest {
    LlmRequest::new("schema-model", vec![Content::new("user").with_text("synthetic")])
        .with_response_schema(schema)
}

async fn endpoint(server: &MockServer, streaming: bool) {
    let response = if streaming {
        let chunk = json!({
            "id":"synthetic", "object":"chat.completion.chunk", "created":1,
            "model":"schema-model",
            "choices":[{"index":0,"delta":{"role":"assistant","content":"{}"},
                        "finish_reason":"stop"}]
        });
        ResponseTemplate::new(200)
            .insert_header("content-type", "text/event-stream")
            .set_body_string(format!("data: {chunk}\n\ndata: [DONE]\n\n"))
    } else {
        ResponseTemplate::new(200).set_body_json(json!({
            "id":"synthetic", "object":"chat.completion", "created":1,
            "model":"schema-model",
            "choices":[{"index":0,"message":{"role":"assistant","content":"{}"},
                        "finish_reason":"stop"}],
            "usage":{"prompt_tokens":1,"completion_tokens":1,"total_tokens":2}
        }))
    };
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(response)
        .mount(server)
        .await;
}

async fn captured_format(
    server: &MockServer,
    model: &OpenAICompatible,
    input: &LlmRequest,
    streaming: bool,
    strict: bool,
) {
    let original = serde_json::to_value(input).expect("input JSON");
    let mut output = model.generate_content(input.clone(), streaming).await.expect("generate");
    let mut count = 0;
    while let Some(item) = output.next().await {
        item.expect("response");
        count += 1;
    }
    assert!(count > 0, "consume actual model output");
    let requests = server.received_requests().await.expect("received");
    assert_eq!(requests.len(), 1);
    let wire: Value = serde_json::from_slice(&requests[0].body).expect("wire JSON");
    assert_eq!(
        wire["response_format"],
        json!({"type":"json_schema","json_schema":{
            "name":"schema_model", "schema":input.config.as_ref().expect("config").response_schema,
            "strict":strict
        }})
    );
    assert_eq!(serde_json::to_value(input).expect("unchanged input"), original);
}

#[tokio::test]
async fn response_schema_is_not_narrowed_on_wire() {
    for streaming in [false, true] {
        for schema in schema_families() {
            let server = MockServer::start().await;
            endpoint(&server, streaming).await;
            let model = client(&server).with_response_schema_strict(false);
            captured_format(&server, &model, &request(schema), streaming, false).await;
        }
    }
}

fn closed_schema() -> Value {
    json!({"type":"object", "properties":{"name":{"type":"string"}},
           "required":["name"], "additionalProperties":false})
}

fn schema_families() -> Vec<Value> {
    vec![
        json!({"$id":"urn:sentinel:synthetic", "enum":[{
            "schema_version":1,"node":"example","source":{"path":"synthetic.rs","line":1}
        }]}),
        json!({"type":"object", "properties":{"name":{"type":"string"}}}),
        json!({"type":"object", "additionalProperties":{"type":"integer"}}),
        closed_schema(),
    ]
}

async fn rejected(model: &OpenAICompatible, input: LlmRequest, streaming: bool) -> AdkError {
    match model.generate_content(input, streaming).await {
        Err(error) => error,
        Ok(mut output) => {
            while let Some(item) = output.next().await {
                if let Err(error) = item {
                    return error;
                }
            }
            panic!("expected admission failure, not a successful stream");
        }
    }
}

#[tokio::test]
async fn default_strict_rejects_open_schema_before_http() {
    for streaming in [false, true] {
        let server = MockServer::start().await;
        endpoint(&server, streaming).await;
        let error = rejected(&client(&server), request(json!({"type":"object"})), streaming).await;
        assert!(server.received_requests().await.expect("received").is_empty());
        assert_eq!(error.category, ErrorCategory::InvalidInput);
        assert_eq!(error.code, "model.openai_compat.response_schema_unsupported");
        assert!(error.message.starts_with("/response_schema/additionalProperties:"));
    }
}

#[tokio::test]
async fn strict_supported_schema_keeps_nested_nullable_and_keyword_named_properties() {
    let schema = json!({
        "type":"object", "description":"synthetic", "properties":{
            "additionalProperties":{"type":["string","null"],"enum":["a",null]},
            "enum":{"type":"array","items":{"type":"integer","enum":[1,2]}},
            "$ref":{"type":["object","null"],"properties":{
                "type":{"type":"boolean"}, "a~/b":{"type":"null"}
            },"required":["type","a~/b"],"additionalProperties":false}
        },"required":["additionalProperties","enum","$ref"],"additionalProperties":false
    });
    for streaming in [false, true] {
        for explicit in [false, true] {
            let server = MockServer::start().await;
            endpoint(&server, streaming).await;
            let model = if explicit {
                client(&server).with_response_schema_strict(true)
            } else {
                client(&server)
            };
            captured_format(&server, &model, &request(schema.clone()), streaming, true).await;
        }
    }
}

#[tokio::test]
async fn unsupported_or_malformed_strict_schemas_have_zero_egress_and_stable_paths() {
    let mut cases = vec![
        (json!({"enum":[{"name":"value"}]}), "/type"),
        (json!(true), ""),
        (json!({"type":["object","null"]}), "/type"),
        (
            json!({"type":"object","properties":{},"required":[],"additionalProperties":{"type":"string"}}),
            "/additionalProperties",
        ),
        (
            json!({"type":"object","properties":{"name":{"type":"string"}},"required":[],"additionalProperties":false}),
            "/required",
        ),
        (
            json!({"type":"object","properties":{"name":{"type":"string"}},"required":["name","name"],"additionalProperties":false}),
            "/required",
        ),
    ];
    for (child, suffix) in [
        (json!({"type":"object","properties":{},"required":[]}), "/additionalProperties"),
        (json!({"type":"string","minLength":1}), "/minLength"),
        (json!({"$ref":"#/$defs/Name"}), "/type"),
        (json!({"type":"array","items":[{"type":"string"}]}), "/items"),
        (json!({"type":"string","enum":["a",1]}), "/enum"),
        (json!({"type":"string","enum":["a","a"]}), "/enum"),
        (json!({"type":"string","description":false}), "/description"),
        (json!({"type":"string","anyOf":[{"type":"string"}]}), "/anyOf"),
        (json!({"type":"string","items":{"type":"string"}}), "/items"),
        (
            json!({"type":"object","properties":{},"required":[],"additionalProperties":true}),
            "/additionalProperties",
        ),
        (
            json!({"type":"object","properties":{},"required":[],"additionalProperties":false,"enum":[{}]}),
            "/enum",
        ),
        (json!({"type":["null","null"]}), "/type"),
        (json!(false), ""),
    ] {
        cases.push((
            json!({"type":"object","properties":{"a~/b":child},
            "required":["a~/b"],"additionalProperties":false}),
            suffix,
        ));
    }
    for streaming in [false, true] {
        for (index, (schema, suffix)) in cases.iter().enumerate() {
            let server = MockServer::start().await;
            endpoint(&server, streaming).await;
            let error = rejected(&client(&server), request(schema.clone()), streaming).await;
            assert_eq!(error.category, ErrorCategory::InvalidInput);
            assert_eq!(error.code, "model.openai_compat.response_schema_unsupported");
            let prefix =
                if index >= 6 { "/response_schema/properties/a~0~1b" } else { "/response_schema" };
            assert!(error.message.starts_with(&format!("{prefix}{suffix}:")), "{}", error.message);
            assert!(server.received_requests().await.expect("received").is_empty());
        }
    }
}

#[tokio::test]
async fn configured_response_format_rejects_namespace_and_adapter_conflicts() {
    for streaming in [false, true] {
        for strict in [false, true] {
            for mutation in 0..5 {
                let server = MockServer::start().await;
                endpoint(&server, streaming).await;
                let mut input = request(closed_schema());
                let mut model = client(&server).with_response_schema_strict(strict);
                if mutation == 0 {
                    input
                        .config
                        .as_mut()
                        .expect("config")
                        .extensions
                        .insert("openai".into(), json!({"response_format":{"type":"text"}}));
                } else {
                    model = model.with_request_adapter(Arc::new(move |body, _| {
                        match mutation {
                            1 => body["response_format"]["json_schema"]["strict"] = json!(!strict),
                            2 => body["response_format"]["json_schema"]["schema"] = json!({}),
                            3 => {
                                body.as_object_mut()
                                    .expect("body object")
                                    .remove("response_format");
                            }
                            _ => *body = json!(null),
                        }
                        Ok(())
                    }));
                }
                let error = rejected(&model, input, streaming).await;
                assert_eq!(error.category, ErrorCategory::InvalidInput);
                assert_eq!(error.code, "model.openai_compat.response_schema_conflict");
                assert!(error.message.starts_with(if mutation == 0 {
                    "/extensions/openai/response_format:"
                } else {
                    "/response_format:"
                }));
                assert!(server.received_requests().await.expect("received").is_empty());
            }
        }
    }
}

#[tokio::test]
async fn unrelated_customization_preserves_configured_schema() {
    for streaming in [false, true] {
        let server = MockServer::start().await;
        endpoint(&server, streaming).await;
        let mut input = request(closed_schema());
        input
            .config
            .as_mut()
            .expect("config")
            .extensions
            .insert("openai".into(), json!({"metadata":{"synthetic":true}}));
        let model = client(&server).with_request_adapter(Arc::new(|body, headers| {
            body["seed"] = json!(7);
            headers.insert("x-synthetic", "true".parse().expect("header"));
            Ok(())
        }));
        captured_format(&server, &model, &input, streaming, true).await;
        let requests = server.received_requests().await.expect("received");
        let wire: Value = serde_json::from_slice(&requests[0].body).expect("wire");
        assert_eq!(wire["seed"], 7);
        assert_eq!(wire["metadata"], json!({"synthetic":true}));
        assert_eq!(requests[0].headers["x-synthetic"], "true");
    }
}

#[tokio::test]
async fn response_schema_mode_is_inert_without_schema() {
    for streaming in [false, true] {
        for strict in [false, true] {
            let server = MockServer::start().await;
            endpoint(&server, streaming).await;
            let mut input = request(closed_schema());
            let config = input.config.as_mut().expect("config");
            config.response_schema = None;
            config.extensions.insert("openai".into(), json!({"response_format":{"type":"text"}}));
            let model = client(&server).with_response_schema_strict(strict).with_request_adapter(
                Arc::new(|body, _| {
                    body["response_format"] = json!({"type":"json_object"});
                    Ok(())
                }),
            );
            let mut output = model.generate_content(input, streaming).await.expect("generate");
            let mut count = 0;
            while let Some(item) = output.next().await {
                item.expect("response");
                count += 1;
            }
            assert!(count > 0);
            let requests = server.received_requests().await.expect("received");
            assert_eq!(requests.len(), 1);
            let wire: Value = serde_json::from_slice(&requests[0].body).expect("wire");
            assert_eq!(wire["response_format"], json!({"type":"json_object"}));
        }
    }
}

#[tokio::test]
async fn numeric_alias_enums_are_rejected_before_http() {
    for streaming in [false, true] {
        for (kind, values) in [
            (json!("integer"), json!([1, 1.0])),
            (json!("number"), json!([1, 1.0])),
            (json!("number"), json!([0, -0.0])),
            (json!(["number", "null"]), json!([null, 1, 1.0])),
            (json!(["integer", "null"]), json!([null, 0, -0.0])),
            (json!("number"), json!([100, 1e2])),
        ] {
            let server = MockServer::start().await;
            endpoint(&server, streaming).await;
            let schema = json!({"type":"object", "properties":{"n":{"type":kind,"enum":values}},
                "required":["n"],"additionalProperties":false});
            let error = rejected(&client(&server), request(schema), streaming).await;
            assert_eq!(error.category, ErrorCategory::InvalidInput);
            assert_eq!(error.code, "model.openai_compat.response_schema_unsupported");
            assert!(error.message.starts_with("/response_schema/properties/n/enum:"));
            assert!(server.received_requests().await.expect("received").is_empty());
        }
    }
}

#[tokio::test]
async fn distinct_large_integer_and_large_enum_schemas_are_preserved() {
    for streaming in [false, true] {
        for values in [
            json!([9007199254740992_u64, 9007199254740993_u64, u64::MAX]),
            Value::Array((0..16384).map(|number| json!(number)).collect()),
        ] {
            let server = MockServer::start().await;
            endpoint(&server, streaming).await;
            let schema = json!({"type":"object", "properties":{"n":{"type":"integer","enum":values}},
                "required":["n"],"additionalProperties":false});
            captured_format(&server, &client(&server), &request(schema), streaming, true).await;
        }
    }
}

#[tokio::test]
async fn arbitrary_precision_integer_contract() {
    // Dependency features are unified downstream, not declared as adk-model flags.
    let probe: Value = serde_json::from_str("1.0000000000000000001").expect("number");
    let probe_token = probe.to_string();
    if probe_token != "1.0000000000000000001" {
        assert_eq!(probe_token, "1.0");
        println!("SDK3_ARBITRARY_PRECISION_INACTIVE");
        return;
    }
    println!("SDK3_ARBITRARY_PRECISION_ACTIVE");
    for streaming in [false, true] {
        for token in [
            "1.0000000000000000001",
            "-1.0000000000000000001",
            "9007199254740992.1",
            "1e-400",
            "-1e-400",
        ] {
            let value: Value = serde_json::from_str(token).expect("exact numeric literal");
            assert!(value.is_number());
            assert_eq!(value.to_string(), token, "number token must survive parsing");
            for kind in [json!("integer"), json!(["integer", "null"])] {
                let server = MockServer::start().await;
                endpoint(&server, streaming).await;
                let schema = json!({"type":"object", "properties":{"n":{"type":kind,"enum":[value.clone()]}},
                    "required":["n"],"additionalProperties":false});
                let error = rejected(&client(&server), request(schema), streaming).await;
                assert_eq!(error.category, ErrorCategory::InvalidInput);
                assert_eq!(error.code, "model.openai_compat.response_schema_unsupported");
                assert!(error.message.starts_with("/response_schema/properties/n/enum:"));
                assert!(server.received_requests().await.expect("received").is_empty());
            }
            let server = MockServer::start().await;
            endpoint(&server, streaming).await;
            let schema = json!({"type":"object", "properties":{"n":{"type":"number","enum":[value]}},
                "required":["n"],"additionalProperties":false});
            captured_format(&server, &client(&server), &request(schema), streaming, true).await;
        }
        for kind in [json!("integer"), json!(["integer", "null"]), json!("number")] {
            let value: Value = serde_json::from_str("1e+400").expect("exact integral exponent");
            assert_eq!(value.to_string(), "1e+400", "number token must survive parsing");
            let server = MockServer::start().await;
            endpoint(&server, streaming).await;
            let schema = json!({"type":"object", "properties":{"n":{"type":kind,"enum":[value]}},
                "required":["n"],"additionalProperties":false});
            captured_format(&server, &client(&server), &request(schema), streaming, true).await;
        }
    }
    println!("SDK3_ARBITRARY_PRECISION_COMPLETE");
}
