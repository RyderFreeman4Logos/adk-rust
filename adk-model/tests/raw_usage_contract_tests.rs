//! Public HTTP/SSE contract for raw usage presence, independent of legacy counters.
#![cfg(feature = "openai")]

use adk_core::{Content, FinishReason, Llm, LlmRequest, LlmResponse};
use adk_model::retry::RetryConfig;
use adk_model::{OpenAICompatible, OpenAICompatibleConfig};
use futures::TryStreamExt;
use serde_json::{Value, json};
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{method, path},
};

fn cases() -> Vec<(&'static str, Option<Value>, [i32; 3])> {
    vec![
        ("omitted", None, [0, 0, 0]),
        ("null", Some(Value::Null), [0, 0, 0]),
        ("empty", Some(json!({})), [0, 0, 0]),
        ("partial", Some(json!({"prompt_tokens": 7})), [7, 0, 0]),
        (
            "wrong_count_types",
            Some(json!({"prompt_tokens":"7", "completion_tokens":false, "total_tokens":[]})),
            [0, 0, 0],
        ),
        ("wrong_usage_type", Some(json!([1, 2, 3])), [0, 0, 0]),
        (
            "zero",
            Some(json!({"prompt_tokens":0, "completion_tokens":0, "total_tokens":0})),
            [0, 0, 0],
        ),
        (
            "positive",
            Some(json!({"prompt_tokens":7, "completion_tokens":3, "total_tokens":10})),
            [7, 3, 10],
        ),
        (
            "wide_integer",
            Some(
                json!({"prompt_tokens":2147483648_u64, "completion_tokens":3, "total_tokens":2147483651_u64}),
            ),
            [0, 3, 0],
        ),
        (
            "negative",
            Some(json!({"prompt_tokens":-1, "completion_tokens":0, "total_tokens":-1})),
            [-1, 0, -1],
        ),
        (
            "fractional",
            Some(json!({"prompt_tokens":1.5, "completion_tokens":0, "total_tokens":1.5})),
            [0, 0, 0],
        ),
        (
            "native_details",
            Some(json!({"prompt_tokens":7, "completion_tokens":3, "total_tokens":10,
            "prompt_tokens_details":{"cached_tokens":0}, "completion_tokens_details":{"reasoning_tokens":2},
            "provider_extra":{"observed":true}})),
            [7, 3, 10],
        ),
    ]
}

fn assert_usage(response: &LlmResponse, supplied: Option<&Value>, normalized: [i32; 3]) {
    let expected = supplied.filter(|usage| usage.is_object());
    let usage = response.usage_metadata.as_ref();
    assert_eq!(usage.is_some(), expected.is_some());
    if let Some(usage) = usage {
        assert_eq!(usage.provider_usage.as_ref(), expected);
        // These compatibility values are not evidence of observed counts.
        assert_eq!(
            [usage.prompt_token_count, usage.candidates_token_count, usage.total_token_count],
            normalized
        );
        assert_eq!(
            usage.cache_read_input_token_count,
            expected
                .and_then(|v| v.pointer("/prompt_tokens_details/cached_tokens"))
                .and_then(Value::as_i64)
                .map(|n| n as i32)
        );
        assert_eq!(
            usage.thinking_token_count,
            expected
                .and_then(|v| v.pointer("/completion_tokens_details/reasoning_tokens"))
                .and_then(Value::as_i64)
                .map(|n| n as i32)
        );
    }
    let encoded = serde_json::to_value(response).unwrap();
    let decoded: LlmResponse = serde_json::from_value(encoded.clone()).unwrap();
    assert_eq!(serde_json::to_value(decoded).unwrap(), encoded);
}

async fn receive(body: ResponseTemplate, streaming: bool) -> (Vec<LlmResponse>, Value) {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(body)
        .expect(1)
        .mount(&server)
        .await;
    let model = OpenAICompatible::new(
        OpenAICompatibleConfig::new("synthetic-key", "synthetic-model").with_base_url(server.uri()),
    )
    .unwrap()
    .with_retry_config(RetryConfig::disabled());
    let request = LlmRequest::new("synthetic-model", vec![Content::new("user").with_text("Hello")]);
    let responses = model
        .generate_content(request, streaming)
        .await
        .unwrap()
        .try_collect::<Vec<_>>()
        .await
        .unwrap();
    let requests = server.received_requests().await.unwrap();
    assert_eq!(requests.len(), 1);
    let sent = serde_json::from_slice(&requests[0].body).unwrap();
    (responses, sent)
}

#[tokio::test]
async fn http_raw_usage_presence_matrix() {
    for (case, usage, normalized) in cases() {
        let mut body = json!({"choices":[{"message":{"role":"assistant","content":"Hello"},"finish_reason":"stop"}]});
        if let Some(usage) = &usage {
            body["usage"] = usage.clone();
        }
        let (responses, _) = receive(ResponseTemplate::new(200).set_body_json(body), false).await;
        assert_eq!(responses.len(), 1, "{case}");
        assert!(responses[0].turn_complete && !responses[0].partial, "{case}");
        assert_eq!(responses[0].finish_reason, Some(FinishReason::Stop), "{case}");
        assert_eq!(
            serde_json::to_value(&responses[0].content).unwrap(),
            serde_json::to_value(Some(Content::new("model").with_text("Hello"))).unwrap(),
            "{case}"
        );
        assert_usage(&responses[0], usage.as_ref(), normalized);
    }
}

fn sse(usage: Option<&Value>, trailer: bool, done: bool) -> String {
    // Intermediate usage is cumulative; it must not be added to the final snapshot.
    let mut body = format!(
        "data: {}\n\ndata: {}\n\n",
        json!({"choices":[{"delta":{"content":"Hello"}}], "usage":{"prompt_tokens":99,"completion_tokens":99,"total_tokens":198}}),
        json!({"choices":[{"delta":{},"finish_reason":"stop"}]})
    );
    if trailer {
        let mut chunk = json!({"choices":[]});
        if let Some(usage) = usage {
            chunk["usage"] = usage.clone();
        }
        body.push_str(&format!("data: {chunk}\n\n"));
    }
    if done {
        body.push_str("data: [DONE]\n\n");
    }
    body
}

#[tokio::test]
async fn sse_final_trailer_raw_usage_presence_matrix() {
    for (case, usage, normalized) in cases() {
        let body = sse(usage.as_ref(), true, true);
        let (responses, sent) = receive(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(body),
            true,
        )
        .await;
        assert_eq!(sent["stream"], true);
        assert_eq!(sent["stream_options"], json!({"include_usage":true}));
        assert_eq!(responses.len(), 2, "{case}");
        assert!(responses[0].partial && !responses[0].turn_complete, "{case}");
        assert!(responses[0].usage_metadata.is_none(), "{case}");
        assert_eq!(
            serde_json::to_value(&responses[0].content).unwrap(),
            serde_json::to_value(Some(Content::new("model").with_text("Hello"))).unwrap(),
            "{case}"
        );
        let last = &responses[1];
        assert!(last.turn_complete && !last.partial, "{case}");
        assert_eq!(last.finish_reason, Some(FinishReason::Stop), "{case}");
        assert_usage(last, usage.as_ref(), normalized);
        assert_eq!(
            responses.iter().filter(|response| response.usage_metadata.is_some()).count(),
            usize::from(usage.as_ref().is_some_and(Value::is_object)),
            "{case}"
        );
    }
}

#[tokio::test]
async fn sse_missing_or_unfinished_trailer_does_not_invent_usage() {
    for body in [
        sse(None, false, true),
        sse(None, false, false),
        format!("{}data: {{\"choices\":[],\"usage\":", sse(None, false, false)),
    ] {
        let (responses, _) = receive(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(body),
            true,
        )
        .await;
        assert_eq!(responses.len(), 2);
        assert!(responses.last().unwrap().turn_complete);
        assert!(responses.iter().all(|response| response.usage_metadata.is_none()));
    }
}
