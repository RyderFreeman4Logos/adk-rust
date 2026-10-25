//! Public HTTP contracts for the opt-in non-streaming body limit.
#![cfg(feature = "openai")]

use adk_core::{AdkError, Content, ErrorCategory, ErrorComponent, Llm, LlmRequest, LlmResponse};
use adk_model::retry::RetryConfig;
use adk_model::{OpenAICompatible, OpenAICompatibleConfig};
use futures::StreamExt;
use std::num::NonZeroUsize;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn client(url: &str) -> OpenAICompatible {
    OpenAICompatible::new_no_auth(
        OpenAICompatibleConfig::new("", "body-limit-model")
            .with_provider_name("fixture")
            .with_base_url(url),
    )
    .unwrap()
    .with_retry_config(RetryConfig::disabled())
}

fn response_body() -> String {
    serde_json::json!({
        "choices": [{"index": 0, "message": {"role": "assistant", "content": "ok"}, "finish_reason": "stop"}],
        "ignored": "sensitive-body-sentinel-🙂"
    }).to_string()
}

async fn collect(client: &OpenAICompatible, stream: bool) -> Result<Vec<LlmResponse>, AdkError> {
    let request =
        LlmRequest::new("body-limit-model", vec![Content::new("user").with_text("hello")]);
    let mut output = client.generate_content(request, stream).await?;
    let mut responses = Vec::new();
    while let Some(item) = output.next().await {
        responses.push(item?);
    }
    Ok(responses)
}

fn assert_limit(error: AdkError) {
    assert_eq!(
        (
            error.component,
            error.category,
            error.code,
            error.retry.should_retry,
            error.details.provider.as_deref()
        ),
        (
            ErrorComponent::Model,
            ErrorCategory::Internal,
            "model.openai_compat.response_body_too_large",
            false,
            Some("fixture")
        )
    );
    assert!(!format!("{error:?} {error}").contains("sensitive-body-sentinel"));
}

#[tokio::test]
async fn non_streaming_body_byte_overflow_is_refused() {
    let server = MockServer::start().await;
    let body = response_body();
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_string(body.clone()))
        .expect(1)
        .mount(&server)
        .await;
    let client =
        client(&server.uri()).with_max_response_bytes(NonZeroUsize::new(body.len() - 1).unwrap());
    assert_limit(
        collect(&client, false)
            .await
            .expect_err("configured body byte overflow must fail before JSON output"),
    );
}

#[tokio::test]
async fn exact_limit_and_unlimited_default_preserve_valid_responses() {
    let server = MockServer::start().await;
    let body = response_body();
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_string(body.clone()))
        .expect(3)
        .mount(&server)
        .await;
    let unlimited = collect(&client(&server.uri()), false).await.unwrap();
    assert_eq!(unlimited.len(), 1);
    for limit in [body.len(), body.len() + 1] {
        let limited =
            client(&server.uri()).with_max_response_bytes(NonZeroUsize::new(limit).unwrap());
        assert_eq!(
            serde_json::to_value(collect(&limited, false).await.unwrap()).unwrap(),
            serde_json::to_value(&unlimited).unwrap()
        );
    }
}

#[tokio::test]
async fn header_builder_order_preserves_the_limit() {
    let server = MockServer::start().await;
    let body = response_body();
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_string(body.clone()))
        .expect(2)
        .mount(&server)
        .await;
    let limit = NonZeroUsize::new(body.len() - 1).unwrap();
    let headers = http::HeaderMap::new();
    let before = client(&server.uri())
        .with_default_headers(headers.clone())
        .unwrap()
        .with_max_response_bytes(limit);
    let after =
        client(&server.uri()).with_max_response_bytes(limit).with_default_headers(headers).unwrap();
    for client in [before, after] {
        assert_limit(collect(&client, false).await.unwrap_err());
    }
}

#[tokio::test]
async fn error_body_overflow_is_not_retried() {
    let server = MockServer::start().await;
    for status in [500, 429, 503] {
        Mock::given(method("POST"))
            .respond_with(
                ResponseTemplate::new(status)
                    .set_body_string(response_body())
                    .insert_header("Retry-After", "1"),
            )
            .expect(1)
            .mount(&server)
            .await;
        let client = client(&server.uri())
            .with_retry_config(RetryConfig::default())
            .with_max_response_bytes(NonZeroUsize::new(8).unwrap());
        assert_limit(collect(&client, false).await.unwrap_err());
        assert_eq!(server.received_requests().await.unwrap().len(), 1, "status {status}");
        server.reset().await;
    }
}

#[tokio::test]
async fn unlimited_default_accepts_payload_larger_than_an_opt_in_limit() {
    let server = MockServer::start().await;
    let mut body: serde_json::Value = serde_json::from_str(&response_body()).unwrap();
    body["ignored"] = serde_json::json!("x".repeat(8192));
    let body = body.to_string();
    assert!(body.len() > 8192);
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_string(body))
        .expect(2)
        .mount(&server)
        .await;
    assert_eq!(collect(&client(&server.uri()), false).await.unwrap().len(), 1);
    let limited = client(&server.uri()).with_max_response_bytes(NonZeroUsize::new(64).unwrap());
    assert_limit(collect(&limited, false).await.unwrap_err());
}

#[tokio::test]
async fn under_limit_status_retry_after_and_json_errors_are_preserved() {
    let server = MockServer::start().await;
    for (status, body, code, category, retry, delay) in [
        (403, "denied", "model.openai_compat.api_error", ErrorCategory::Forbidden, false, None),
        (
            429,
            "busy",
            "model.openai_compat.api_error",
            ErrorCategory::RateLimited,
            true,
            Some(2000),
        ),
        (200, "not json", "model.openai_compat.parse", ErrorCategory::Internal, false, None),
    ] {
        Mock::given(method("POST"))
            .respond_with(
                ResponseTemplate::new(status)
                    .set_body_string(body)
                    .insert_header("Retry-After", "2"),
            )
            .expect(1)
            .mount(&server)
            .await;
        let client = client(&server.uri()).with_max_response_bytes(NonZeroUsize::new(64).unwrap());
        let error = collect(&client, false).await.unwrap_err();
        assert_eq!(
            (error.code, error.category, error.retry.should_retry, error.retry.retry_after_ms),
            (code, category, retry, delay)
        );
        server.reset().await;
    }
}

#[tokio::test]
async fn capped_client_keeps_streaming_unchanged() {
    let server = MockServer::start().await;
    let client = client(&server.uri()).with_max_response_bytes(NonZeroUsize::new(1).unwrap());
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_string(response_body()))
        .expect(1)
        .mount(&server)
        .await;
    assert_limit(collect(&client, false).await.unwrap_err());
    server.reset().await;
    let event = serde_json::json!({"choices":[{"index":0,"delta":{"content":"streamed text"},"finish_reason":null}]}).to_string();
    let finish =
        serde_json::json!({"choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}).to_string();
    let body = format!("data: {event}\n\ndata: {finish}\n\ndata: [DONE]\n\n");
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(body, "text/event-stream"))
        .expect(1)
        .mount(&server)
        .await;
    let responses = collect(&client, true).await.unwrap();
    assert!(responses.iter().any(|r| r.content.as_ref().is_some_and(|c| {
        c.parts.iter().any(|p| matches!(p, adk_core::Part::Text {text} if text == "streamed text"))
    })));
    server.reset().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(403).set_body_string("stream error body"))
        .expect(1)
        .mount(&server)
        .await;
    let error = collect(&client, true).await.unwrap_err();
    assert_eq!(error.code, "model.openai_compat.api_error");
    assert!(error.message.contains("stream error body"));
}

// Raw framing is needed only where the HTTP mock supplies its own length.
struct RawServer {
    url: String,
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
    requests: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    thread: Option<std::thread::JoinHandle<std::io::Result<()>>>,
}

impl RawServer {
    fn start(reply: Vec<u8>, withhold_body: bool) -> Self {
        Self::sequence(vec![reply], withhold_body)
    }

    fn sequence(replies: Vec<Vec<u8>>, withhold_body: bool) -> Self {
        use std::io::{Read, Write};
        use std::sync::atomic::Ordering;
        use std::time::{Duration, Instant};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let worker_stop = stop.clone();
        let requests = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let worker_requests = requests.clone();
        let thread = std::thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(3);
            for reply in replies {
                let mut stream = loop {
                    if worker_stop.load(Ordering::Acquire) {
                        return Ok(());
                    }
                    match listener.accept() {
                        Ok((stream, _)) => break stream,
                        Err(e)
                            if e.kind() == std::io::ErrorKind::WouldBlock
                                && Instant::now() < deadline =>
                        {
                            std::thread::sleep(Duration::from_millis(1))
                        }
                        Err(e) => return Err(e),
                    }
                };
                stream.set_read_timeout(Some(Duration::from_millis(200)))?;
                stream.set_write_timeout(Some(Duration::from_millis(200)))?;
                let mut request = Vec::new();
                loop {
                    let mut bytes = [0; 1024];
                    let count = stream.read(&mut bytes)?;
                    if count == 0 || request.len() + count > 16_384 {
                        return Err(std::io::Error::other(
                            "incomplete or oversized fixture request",
                        ));
                    }
                    request.extend_from_slice(&bytes[..count]);
                    if let Some(end) = request.windows(4).position(|w| w == b"\r\n\r\n") {
                        let headers = String::from_utf8_lossy(&request[..end]);
                        let length = headers
                            .lines()
                            .find_map(|line| {
                                let (name, value) = line.split_once(':')?;
                                name.eq_ignore_ascii_case("content-length")
                                    .then(|| value.trim().parse::<usize>().ok())
                                    .flatten()
                            })
                            .unwrap_or(0);
                        if request.len() >= end + 4 + length {
                            break;
                        }
                    }
                    if Instant::now() >= deadline || worker_stop.load(Ordering::Acquire) {
                        return Err(std::io::Error::other("fixture request did not complete"));
                    }
                }
                worker_requests.fetch_add(1, Ordering::Release);
                stream.write_all(&reply)?;
                stream.flush()?;
                while withhold_body
                    && !worker_stop.load(Ordering::Acquire)
                    && Instant::now() < deadline
                {
                    std::thread::sleep(Duration::from_millis(1));
                }
            }
            Ok(())
        });
        Self { url, stop, requests, thread: Some(thread) }
    }
}

impl Drop for RawServer {
    fn drop(&mut self) {
        self.stop.store(true, std::sync::atomic::Ordering::Release);
        if let Some(thread) = self.thread.take() {
            let result = thread.join();
            if !std::thread::panicking() {
                assert!(matches!(result, Ok(Ok(()))), "raw HTTP fixture failed: {result:?}");
            }
        }
    }
}

#[tokio::test]
async fn chunked_body_is_counted_without_content_length() {
    let body = response_body();
    let midpoint = body.len() / 2;
    let mut wire =
        b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n".to_vec();
    for chunk in [&body.as_bytes()[..midpoint], &body.as_bytes()[midpoint..]] {
        wire.extend_from_slice(format!("{:x}\r\n", chunk.len()).as_bytes());
        wire.extend_from_slice(chunk);
        wire.extend_from_slice(b"\r\n");
    }
    // Keep the terminal chunk absent: overflow must stop reading before EOF.
    let server = RawServer::start(wire, true);
    let client =
        client(&server.url).with_max_response_bytes(NonZeroUsize::new(body.len() - 1).unwrap());
    let result = tokio::time::timeout(std::time::Duration::from_secs(1), collect(&client, false))
        .await
        .expect("chunked overflow must not wait for the terminal chunk");
    assert_limit(result.unwrap_err());
}

#[tokio::test]
async fn oversized_declared_length_is_refused_before_body_arrives() {
    let server = RawServer::start(
        b"HTTP/1.1 200 OK\r\nContent-Length: 5000\r\nConnection: close\r\n\r\n".to_vec(),
        true,
    );
    let client = client(&server.url).with_max_response_bytes(NonZeroUsize::new(64).unwrap());
    let result = tokio::time::timeout(std::time::Duration::from_secs(1), collect(&client, false))
        .await
        .expect("known oversized length must not wait for the withheld body");
    assert_limit(result.unwrap_err());
}

#[tokio::test]
async fn incorrect_lengths_follow_http_framing_and_never_yield_partial_json() {
    let body = response_body();
    for length in [2, body.len() + 5] {
        let wire = format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {length}\r\nConnection: close\r\n\r\n{body}"
        )
        .into_bytes();
        let server = RawServer::start(wire, false);
        let client = client(&server.url)
            .with_max_response_bytes(NonZeroUsize::new(body.len() + 10).unwrap());
        let error = collect(&client, false).await.unwrap_err();
        assert_eq!(error.code, "model.openai_compat.parse");
    }
}

#[tokio::test]
async fn truncated_error_bodies_preserve_status_and_retry_guidance() {
    for (status, category, retry, delay) in [
        (429, ErrorCategory::RateLimited, true, Some(2000)),
        (503, ErrorCategory::Unavailable, true, Some(2000)),
        (403, ErrorCategory::Forbidden, false, None),
    ] {
        let mut errors = Vec::new();
        for limited in [false, true] {
            let wire = format!(
                "HTTP/1.1 {status} Fixture\r\nContent-Length: 10\r\nRetry-After: 2\r\nConnection: close\r\n\r\nx"
            );
            let server = RawServer::start(wire.into_bytes(), false);
            let mut client = client(&server.url);
            if limited {
                client = client.with_max_response_bytes(NonZeroUsize::new(64).unwrap());
            }
            let error = collect(&client, false).await.unwrap_err();
            assert_eq!(
                (
                    error.code,
                    error.category,
                    error.retry.should_retry,
                    error.retry.retry_after_ms,
                    error.details.upstream_status_code,
                    error.details.provider.as_deref(),
                ),
                (
                    "model.openai_compat.api_error",
                    category,
                    retry,
                    delay,
                    Some(status),
                    Some("fixture"),
                ),
                "status {status}, limited {limited}"
            );
            errors.push(error.message);
        }
        assert_eq!(errors[0], errors[1]);
    }
}

#[tokio::test]
async fn truncated_transient_error_retries_the_next_physical_request() {
    let body = response_body();
    let server = RawServer::sequence(
        vec![
            b"HTTP/1.1 503 Service Unavailable\r\nContent-Length: 10\r\nRetry-After: 2\r\nConnection: close\r\n\r\nx".to_vec(),
            format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).into_bytes(),
        ],
        false,
    );
    let client = client(&server.url)
        .with_retry_config(
            RetryConfig::default()
                .with_max_retries(1)
                .with_initial_delay(std::time::Duration::from_millis(1))
                .with_max_delay(std::time::Duration::from_millis(1)),
        )
        .with_max_response_bytes(NonZeroUsize::new(body.len()).unwrap());
    let responses =
        tokio::time::timeout(std::time::Duration::from_secs(1), collect(&client, false))
            .await
            .expect("transient status retry must respect the configured delay cap")
            .unwrap();
    assert_eq!(responses.len(), 1);
    assert_eq!(server.requests.load(std::sync::atomic::Ordering::Acquire), 2);
}

#[tokio::test]
async fn capped_latin1_error_matches_uncapped_transport_decoding() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(
            ResponseTemplate::new(403)
                .set_body_raw(vec![b'c', b'a', b'f', 0xe9], "text/plain; charset=iso-8859-1"),
        )
        .expect(2)
        .mount(&server)
        .await;
    // The uncapped transport defines decoding for the active reqwest features.
    let unlimited = collect(&client(&server.uri()), false).await.unwrap_err();
    let limited = client(&server.uri()).with_max_response_bytes(NonZeroUsize::new(4).unwrap());
    let error = collect(&limited, false).await.unwrap_err();
    assert_eq!(error.message, unlimited.message);
}

#[tokio::test]
async fn capped_bom_and_invalid_utf8_errors_match_uncapped_transport_decoding() {
    let server = MockServer::start().await;
    for (body, expected) in [(vec![0xef, 0xbb, 0xbf, b'o', b'k'], "ok"), (vec![b'x', 0xff], "x�")]
    {
        Mock::given(method("POST"))
            .respond_with(
                ResponseTemplate::new(403).set_body_raw(body.clone(), "text/plain; charset=utf-8"),
            )
            .expect(2)
            .mount(&server)
            .await;
        let unlimited = collect(&client(&server.uri()), false).await.unwrap_err();
        assert!(unlimited.message.ends_with(expected));
        // BOM handling must match the uncapped transport's enabled features.
        let limited =
            client(&server.uri()).with_max_response_bytes(NonZeroUsize::new(body.len()).unwrap());
        assert_eq!(collect(&limited, false).await.unwrap_err().message, unlimited.message);
        server.reset().await;
    }
}
