#![cfg(feature = "openai")]

use adk_core::{Content, FinishReason, Llm, LlmRequest, Part};
use adk_model::retry::RetryConfig;
use adk_model::{OpenAICompatible, OpenAICompatibleConfig};
use futures::TryStreamExt;
use serde_json::json;
use std::io::Write;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use wiremock::{Mock, MockServer, ResponseTemplate, matchers::method};

async fn assert_terminal_reasoning(field: &str, tool: bool, mixed_tool: bool) {
    for usage_mode in ["inline", "trailer", "absent"] {
        let server = MockServer::start().await;
        let mut first = json!({field: "first synthetic thought"});
        let mut last = json!({field: "last synthetic thought"});
        if tool {
            first["tool_calls"] = json!([{"index": 0, "id": "synthetic-call", "type": "function",
                "function": {"name": "synthetic_tool", "arguments": "{\"value\":"}}]);
            last["tool_calls"] = json!([{"index": 0, "function": {"arguments": "7}"}}]);
        }
        if !tool || mixed_tool {
            first["content"] = json!("visible-");
            last["content"] = json!("answer");
        }
        // A present primary field retains precedence over the alternate field.
        if field == "reasoning_content" {
            first["reasoning"] = json!("unselected synthetic thought");
            last["reasoning"] = json!("unselected synthetic thought");
        }
        let usage = json!({"prompt_tokens": 2, "completion_tokens": 3, "total_tokens": 5,
            "completion_tokens_details": {"reasoning_tokens": 1}});
        let finish = if tool { "tool_calls" } else { "stop" };
        let mut terminal =
            json!({"choices": [{"index": 0, "delta": last, "finish_reason": finish}]});
        if usage_mode == "inline" {
            terminal["usage"] = usage.clone();
        }
        let mut sse = format!(
            "data: {}\n\ndata: {terminal}\n\n",
            json!({"choices": [{"index": 0, "delta": first, "finish_reason": null}]})
        );
        if usage_mode == "trailer" {
            sse.push_str(&format!("data: {}\n\n", json!({"choices": [], "usage": usage})));
        }
        sse.push_str("data: [DONE]\n\n");
        Mock::given(method("POST"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_string(sse),
            )
            .expect(1)
            .mount(&server)
            .await;
        let client = OpenAICompatible::new_no_auth(
            OpenAICompatibleConfig::new("", "synthetic-model").with_base_url(server.uri()),
        )
        .unwrap()
        .with_retry_config(RetryConfig::disabled());
        let request = LlmRequest::new(
            "synthetic-model",
            vec![
                Content::new("model").with_thinking("synthetic history"),
                Content::new("user").with_text("hello"),
            ],
        );
        let responses = tokio::time::timeout(Duration::from_secs(5), async {
            client
                .generate_content(request, true)
                .await
                .unwrap()
                .try_collect::<Vec<_>>()
                .await
                .unwrap()
        })
        .await
        .expect("local stream must finish");

        let mut expected =
            vec![Part::Thinking { thinking: "first synthetic thought".into(), signature: None }];
        if !tool || mixed_tool {
            expected.push(Part::Text { text: "visible-".into() });
        }
        expected
            .push(Part::Thinking { thinking: "last synthetic thought".into(), signature: None });
        if mixed_tool {
            expected.push(Part::Text { text: "answer".into() });
        }
        expected.push(if tool {
            Part::FunctionCall {
                name: "synthetic_tool".into(),
                args: json!({"value": 7}),
                id: Some("synthetic-call".into()),
                thought_signature: None,
            }
        } else {
            Part::Text { text: "answer".into() }
        });
        let parts: Vec<_> = responses
            .iter()
            .filter_map(|r| r.content.as_ref())
            .flat_map(|c| c.parts.clone())
            .collect();
        assert_eq!(parts, expected, "field={field}, tool={tool}, usage={usage_mode}");
        assert_eq!(responses.len(), expected.len() - usize::from(mixed_tool));
        for response in &responses[..responses.len() - 1] {
            assert!(response.partial);
            assert!(!response.turn_complete);
            assert!(response.finish_reason.is_none());
            assert!(response.usage_metadata.is_none());
        }
        let final_response = responses.last().unwrap();
        let final_parts = 1 + usize::from(mixed_tool);
        assert_eq!(
            final_response.content.as_ref().unwrap().parts.as_slice(),
            &expected[expected.len() - final_parts..]
        );
        assert!(!final_response.partial);
        assert_eq!(final_response.turn_complete, !tool);
        assert_eq!(final_response.finish_reason, Some(FinishReason::Stop));
        let usages: Vec<_> = responses.iter().filter_map(|r| r.usage_metadata.as_ref()).collect();
        if usage_mode == "absent" {
            assert!(usages.is_empty());
        } else {
            assert_eq!(usages.len(), 1);
            assert_eq!(
                (
                    usages[0].prompt_token_count,
                    usages[0].candidates_token_count,
                    usages[0].total_token_count,
                    usages[0].thinking_token_count
                ),
                (2, 3, 5, Some(1))
            );
        }
        let received = server.received_requests().await.unwrap();
        assert_eq!(received.len(), 1);
        let body: serde_json::Value = serde_json::from_slice(&received[0].body).unwrap();
        assert!(
            body["messages"]
                .as_array()
                .unwrap()
                .iter()
                .all(|m| m.get("reasoning_content").is_none() && m.get("reasoning").is_none())
        );
    }
}

#[tokio::test]
async fn terminal_text_preserves_reasoning_content() {
    assert_terminal_reasoning("reasoning_content", false, false).await;
}

#[tokio::test]
async fn terminal_text_preserves_reasoning_fallback() {
    assert_terminal_reasoning("reasoning", false, false).await;
}

#[tokio::test]
async fn terminal_tool_preserves_reasoning_content() {
    assert_terminal_reasoning("reasoning_content", true, false).await;
}

#[tokio::test]
async fn terminal_tool_preserves_reasoning_fallback() {
    assert_terminal_reasoning("reasoning", true, false).await;
}

#[tokio::test]
async fn terminal_native_tools_preserve_text_and_reasoning_content() {
    assert_terminal_reasoning("reasoning_content", true, true).await;
}

#[tokio::test]
async fn terminal_native_tools_preserve_text_and_reasoning_fallback() {
    assert_terminal_reasoning("reasoning", true, true).await;
}

#[tokio::test]
async fn ordinary_native_tools_preserve_mixed_parts() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "choices": [{"message": {"role": "assistant", "content": "answer",
                "tool_calls": [{"id": "synthetic-call", "type": "function",
                    "function": {"name": "synthetic_tool", "arguments": "{\"value\":7}"}}]},
                "finish_reason": "tool_calls"}],
            "usage": {"prompt_tokens": 2, "completion_tokens": 3, "total_tokens": 5}
        })))
        .expect(1)
        .mount(&server)
        .await;
    let client = OpenAICompatible::new_no_auth(
        OpenAICompatibleConfig::new("", "synthetic-model").with_base_url(server.uri()),
    )
    .unwrap()
    .with_retry_config(RetryConfig::disabled());
    let responses = tokio::time::timeout(Duration::from_secs(5), async {
        client
            .generate_content(
                LlmRequest::new("synthetic-model", vec![Content::new("user").with_text("hello")]),
                false,
            )
            .await
            .unwrap()
            .try_collect::<Vec<_>>()
            .await
            .unwrap()
    })
    .await
    .expect("local response must finish");
    assert_eq!(responses.len(), 1);
    let response = &responses[0];
    assert_eq!(
        response.content.as_ref().unwrap().parts,
        vec![
            Part::Text { text: "answer".into() },
            Part::FunctionCall {
                name: "synthetic_tool".into(),
                args: json!({"value": 7}),
                id: Some("synthetic-call".into()),
                thought_signature: None,
            },
        ]
    );
    assert!(!response.partial);
    assert!(!response.turn_complete);
    assert_eq!(response.finish_reason, Some(FinishReason::Stop));
    assert_eq!(response.usage_metadata.as_ref().unwrap().total_token_count, 5);
    assert_eq!(server.received_requests().await.unwrap().len(), 1);
}

#[derive(Clone)]
struct CapturedLog(Arc<Mutex<Vec<u8>>>);

impl Write for CapturedLog {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().write(bytes)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[tokio::test(flavor = "current_thread")]
async fn malformed_sse_diagnostics_exclude_response_payloads() {
    let capture = CapturedLog(Arc::new(Mutex::new(Vec::new())));
    let writer = capture.clone();
    let subscriber = tracing_subscriber::fmt()
        .with_ansi(false)
        .without_time()
        .with_max_level(tracing::Level::WARN)
        .with_writer(move || writer.clone())
        .finish();
    let _guard = tracing::subscriber::set_default(subscriber);
    tracing::warn!("synthetic capture is active");
    let server = MockServer::start().await;
    let marker = "SYNTHETIC_MALFORMED_FRAME_MARKER";
    // A valid JSON string with trailing garbage also exercises content-bearing parser errors.
    let malformed = format!("\"{marker}\" trailing");
    let sse = format!(
        "data: {malformed}\n\ndata: {}\n\ndata: [DONE]\n\n",
        json!({
            "choices": [{"delta": {"content": "surviving synthetic text"}, "finish_reason": "stop"}],
            "usage": {"prompt_tokens": 2, "completion_tokens": 3, "total_tokens": 5}
        })
    );
    Mock::given(method("POST"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(sse),
        )
        .expect(1)
        .mount(&server)
        .await;
    let client = OpenAICompatible::new_no_auth(
        OpenAICompatibleConfig::new("", "synthetic-model").with_base_url(server.uri()),
    )
    .unwrap()
    .with_retry_config(RetryConfig::disabled());
    let responses = tokio::time::timeout(Duration::from_secs(5), async {
        client
            .generate_content(
                LlmRequest::new("synthetic-model", vec![Content::new("user").with_text("hello")]),
                true,
            )
            .await
            .unwrap()
            .try_collect::<Vec<_>>()
            .await
            .unwrap()
    })
    .await
    .expect("local stream must finish");
    assert_eq!(responses.len(), 1);
    assert_eq!(
        responses[0].content.as_ref().unwrap().parts,
        vec![Part::Text { text: "surviving synthetic text".into() }]
    );
    assert!(!responses[0].partial);
    assert!(responses[0].turn_complete);
    assert_eq!(responses[0].finish_reason, Some(FinishReason::Stop));
    assert_eq!(responses[0].usage_metadata.as_ref().unwrap().total_token_count, 5);
    assert_eq!(server.received_requests().await.unwrap().len(), 1);
    let logs = String::from_utf8(capture.0.lock().unwrap().clone()).unwrap();
    assert!(logs.contains("synthetic capture is active"), "capture must observe warnings");
    assert!(logs.contains("failed to parse SSE chunk"), "parse diagnostics must remain");
    assert!(!logs.contains(marker), "malformed response content reached tracing");
    assert!(!logs.contains(&malformed));
    assert!(!logs.contains("surviving synthetic text"));
    assert!(logs.contains("error.category="));
    assert!(logs.contains("error.line="));
    assert!(logs.contains("error.column="));
    assert!(logs.len() < 1024, "parse diagnostics must remain bounded");
}
