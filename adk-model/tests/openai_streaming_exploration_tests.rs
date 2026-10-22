//! Public SSE regression contracts for OpenAI-compatible Chat Completions.
//!
//! Verify incremental content/tools, final usage and allowlisted observations,
//! including byte-fragmented and interrupted response boundaries.

#[cfg(feature = "openai")]
mod streaming_exploration {
    use adk_core::{Content, Llm, LlmRequest, Part};
    use adk_model::openai_compatible::{OpenAICompatible, OpenAICompatibleConfig};
    use futures::StreamExt;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    /// Helper: create an `OpenAICompatible` client pointed at the mock server.
    fn make_client(base_url: &str) -> OpenAICompatible {
        let config = OpenAICompatibleConfig::new("test-key", "gpt-4o")
            .with_provider_name("test")
            .with_base_url(base_url);
        OpenAICompatible::new(config).expect("client creation should succeed")
    }

    /// Helper: build a minimal `LlmRequest`.
    fn make_request() -> LlmRequest {
        LlmRequest::new("gpt-4o", vec![Content::new("user").with_text("Hello")])
    }

    // ── Test 1: stream flag in request body ─────────────────────────────
    //
    // **Validates: Requirements 1.1, 2.1**
    //
    // When `stream=true` is passed to `generate_content()`, the HTTP request
    // body MUST contain `"stream": true`. On unfixed code the `_stream`
    // parameter is ignored, so this field will be absent → test FAILS.

    #[tokio::test]
    async fn stream_flag_present_in_request_body() {
        let server = MockServer::start().await;

        // Return a valid non-streaming response so the call doesn't error out.
        // The important thing is capturing what was SENT.
        let body = serde_json::json!({
            "id": "chatcmpl-1",
            "object": "chat.completion",
            "choices": [{
                "index": 0,
                "message": { "role": "assistant", "content": "Hi" },
                "finish_reason": "stop"
            }],
            "usage": { "prompt_tokens": 5, "completion_tokens": 1, "total_tokens": 6 }
        });

        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(&body))
            .expect(1)
            .mount(&server)
            .await;

        let client = make_client(&server.uri());
        let request = make_request();

        // Call with stream=true
        let stream_result = client.generate_content(request, true).await;
        assert!(stream_result.is_ok(), "generate_content should not error");

        // Consume the stream
        let mut stream = stream_result.unwrap();
        while (stream.next().await).is_some() {}

        // Inspect the captured request
        let received = server.received_requests().await.unwrap();
        assert_eq!(received.len(), 1, "expected exactly one request");

        let req_body: serde_json::Value =
            serde_json::from_slice(&received[0].body).expect("request body should be valid JSON");

        // BUG CONDITION: on unfixed code, "stream" field is absent or false
        assert_eq!(
            req_body.get("stream"),
            Some(&serde_json::json!(true)),
            "request body must contain \"stream\": true when stream=true is passed. \
             Counterexample: request body has no stream field — bug confirmed."
        );
    }

    // ── Test 2: SSE response yields multiple LlmResponse items ──────────
    //
    // **Validates: Requirements 1.2, 1.3, 2.2, 2.3, 2.5**
    //
    // When the server returns an SSE stream with multiple `data:` lines
    // containing `delta.content` chunks, the client MUST yield multiple
    // `LlmResponse` items. On unfixed code, the entire response is parsed
    // as a single JSON blob → only one response yielded → test FAILS.

    #[tokio::test]
    async fn sse_stream_yields_multiple_responses() {
        let server = MockServer::start().await;

        // Build an SSE response with multiple chunks
        let sse_body = [
            "data: {\"id\":\"chatcmpl-1\",\"object\":\"chat.completion.chunk\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"Hello\"},\"finish_reason\":null}]}\n\n",
            "data: {\"id\":\"chatcmpl-1\",\"object\":\"chat.completion.chunk\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\" world\"},\"finish_reason\":null}]}\n\n",
            "data: {\"id\":\"chatcmpl-1\",\"object\":\"chat.completion.chunk\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}],\"usage\":null}\n\n",
            "data: [DONE]\n\n",
        ]
        .join("");

        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_string(&sse_body)
                    .insert_header("content-type", "text/event-stream"),
            )
            .expect(1)
            .mount(&server)
            .await;

        let client = make_client(&server.uri());
        let request = make_request();

        let stream_result = client.generate_content(request, true).await;
        assert!(stream_result.is_ok(), "generate_content should not error");

        let mut stream = stream_result.unwrap();
        let mut responses = Vec::new();
        while let Some(item) = stream.next().await {
            match item {
                Ok(resp) => responses.push(resp),
                Err(e) => panic!("stream yielded error: {e}"),
            }
        }

        // BUG CONDITION: on unfixed code, only 1 response is yielded (the
        // entire SSE body is parsed as a single JSON blob, which fails or
        // produces one response).
        assert!(
            responses.len() >= 2,
            "expected at least 2 LlmResponse items from SSE stream, got {}. \
             Counterexample: single blob response instead of incremental chunks — bug confirmed.",
            responses.len()
        );

        // Check intermediate responses have partial=true, turn_complete=false
        for resp in &responses[..responses.len() - 1] {
            assert!(resp.partial, "intermediate response should have partial=true");
            assert!(!resp.turn_complete, "intermediate response should have turn_complete=false");
        }

        // Check final response has partial=false, turn_complete=true
        let final_resp = responses.last().unwrap();
        assert!(!final_resp.partial, "final response should have partial=false");
        assert!(final_resp.turn_complete, "final response should have turn_complete=true");
    }

    #[tokio::test]
    async fn usage_only_chunk_is_attached_to_the_terminal_response() {
        let server = MockServer::start().await;

        let sse_body = [
            "data: {\"id\":\"chatcmpl-1\",\"object\":\"chat.completion.chunk\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"Hello\"},\"finish_reason\":null}]}\n\n",
            "data: {\"id\":\"chatcmpl-1\",\"object\":\"chat.completion.chunk\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}],\"usage\":null}\n\n",
            "data: {\"id\":\"chatcmpl-1\",\"object\":\"chat.completion.chunk\",\"choices\":[],\"usage\":{\"prompt_tokens\":11,\"completion_tokens\":5,\"total_tokens\":16,\"prompt_tokens_details\":{\"cached_tokens\":7,\"cache_write_tokens\":2},\"provider_meter\":{\"units\":3}}}\n\n",
            "data: [DONE]\n\n",
        ]
        .join("");

        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_string(&sse_body)
                    .insert_header("content-type", "text/event-stream"),
            )
            .expect(1)
            .mount(&server)
            .await;

        let client = make_client(&server.uri());
        let mut stream = client
            .generate_content(make_request(), true)
            .await
            .expect("generate_content should not error");
        let mut responses = Vec::new();

        while let Some(item) = stream.next().await {
            responses.push(item.expect("stream should not yield an error"));
        }

        let terminal_responses: Vec<_> =
            responses.iter().filter(|response| response.turn_complete).collect();
        assert_eq!(terminal_responses.len(), 1);
        let usage = terminal_responses[0]
            .usage_metadata
            .as_ref()
            .expect("usage-only chunk should be attached to the terminal response");
        assert_eq!(usage.prompt_token_count, 11);
        assert_eq!(usage.candidates_token_count, 5);
        assert_eq!(usage.total_token_count, 16);
        assert_eq!(usage.cache_read_input_token_count, Some(7));
        assert_eq!(usage.cache_creation_input_token_count, Some(2));
        assert_eq!(
            usage.provider_usage,
            Some(serde_json::json!({
                "prompt_tokens": 11,
                "completion_tokens": 5,
                "total_tokens": 16,
                "prompt_tokens_details": {
                    "cached_tokens": 7,
                    "cache_write_tokens": 2
                },
                "provider_meter": {"units": 3}
            }))
        );
    }

    // ── Test 3: reasoning_content chunks yield Part::Thinking ────────────
    //
    // **Validates: Requirements 1.2, 2.2**
    //
    // When the SSE stream contains `delta.reasoning_content` chunks, the
    // client MUST yield `LlmResponse` items with `Part::Thinking`. On
    // unfixed code there is no SSE processing → no Part::Thinking from
    // streaming → test FAILS.

    #[tokio::test]
    async fn reasoning_content_chunks_yield_thinking_parts() {
        let server = MockServer::start().await;

        let sse_body = [
            "data: {\"id\":\"chatcmpl-1\",\"object\":\"chat.completion.chunk\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"reasoning_content\":\"Let me think...\"},\"finish_reason\":null}]}\n\n",
            "data: {\"id\":\"chatcmpl-1\",\"object\":\"chat.completion.chunk\",\"choices\":[{\"index\":0,\"delta\":{\"reasoning_content\":\" about this.\"},\"finish_reason\":null}]}\n\n",
            "data: {\"id\":\"chatcmpl-1\",\"object\":\"chat.completion.chunk\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"The answer is 42.\"},\"finish_reason\":null}]}\n\n",
            "data: {\"id\":\"chatcmpl-1\",\"object\":\"chat.completion.chunk\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":5,\"completion_tokens\":10,\"total_tokens\":15}}\n\n",
            "data: [DONE]\n\n",
        ]
        .join("");

        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_string(&sse_body)
                    .insert_header("content-type", "text/event-stream"),
            )
            .expect(1)
            .mount(&server)
            .await;

        let client = make_client(&server.uri());
        let request = make_request();

        let stream_result = client.generate_content(request, true).await;
        assert!(stream_result.is_ok(), "generate_content should not error");

        let mut stream = stream_result.unwrap();
        let mut responses = Vec::new();
        while let Some(item) = stream.next().await {
            match item {
                Ok(resp) => responses.push(resp),
                Err(e) => panic!("stream yielded error: {e}"),
            }
        }

        // Collect all Part::Thinking parts from all responses
        let thinking_parts: Vec<&str> = responses
            .iter()
            .filter_map(|r| r.content.as_ref())
            .flat_map(|c| &c.parts)
            .filter_map(|p| match p {
                Part::Thinking { thinking, .. } => Some(thinking.as_str()),
                _ => None,
            })
            .collect();

        // BUG CONDITION: on unfixed code, no SSE processing occurs, so no
        // Part::Thinking is yielded from streaming delta chunks.
        assert!(
            !thinking_parts.is_empty(),
            "expected Part::Thinking responses from delta.reasoning_content chunks. \
             Counterexample: no Part::Thinking yielded — reasoning content not streamed — bug confirmed."
        );

        // Verify the thinking content matches what was sent
        let combined_thinking: String = thinking_parts.join("");
        assert!(
            combined_thinking.contains("Let me think"),
            "thinking content should contain the reasoning text"
        );
    }

    // ── Test 4: tool_calls chunks accumulated and yielded on finish ──────
    //
    // **Validates: Requirements 1.4, 2.4, 2.5**
    //
    // When the SSE stream contains `delta.tool_calls` chunks, the client
    // MUST accumulate them by index and yield `Part::FunctionCall` on
    // `finish_reason`. On unfixed code there is no SSE processing → test FAILS.

    #[tokio::test]
    async fn tool_call_chunks_accumulated_and_yielded_on_finish() {
        let server = MockServer::start().await;

        let sse_body = [
            // First chunk: tool call index 0, id + function name
            "data: {\"id\":\"chatcmpl-1\",\"object\":\"chat.completion.chunk\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"tool_calls\":[{\"index\":0,\"id\":\"call_abc\",\"type\":\"function\",\"function\":{\"name\":\"get_weather\",\"arguments\":\"\"}}]},\"finish_reason\":null}]}\n\n",
            // Second chunk: tool call index 0, argument fragment
            "data: {\"id\":\"chatcmpl-1\",\"object\":\"chat.completion.chunk\",\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"{\\\"city\\\"\"}}]},\"finish_reason\":null}]}\n\n",
            // Third chunk: tool call index 0, more argument fragment
            "data: {\"id\":\"chatcmpl-1\",\"object\":\"chat.completion.chunk\",\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\": \\\"Paris\\\"}\"}}]},\"finish_reason\":null}]}\n\n",
            // Finish chunk followed by the usage-only chunk required by the OpenAI stream contract.
            "data: {\"id\":\"chatcmpl-1\",\"object\":\"chat.completion.chunk\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"tool_calls\"}],\"usage\":null}\n\n",
            "data: {\"id\":\"chatcmpl-1\",\"object\":\"chat.completion.chunk\",\"choices\":[],\"usage\":{\"prompt_tokens\":10,\"completion_tokens\":15,\"total_tokens\":25}}\n\n",
            "data: [DONE]\n\n",
        ]
        .join("");

        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_string(&sse_body)
                    .insert_header("content-type", "text/event-stream"),
            )
            .expect(1)
            .mount(&server)
            .await;

        let client = make_client(&server.uri());
        let request = make_request();

        let stream_result = client.generate_content(request, true).await;
        assert!(stream_result.is_ok(), "generate_content should not error");

        let mut stream = stream_result.unwrap();
        let mut responses = Vec::new();
        while let Some(item) = stream.next().await {
            match item {
                Ok(resp) => responses.push(resp),
                Err(e) => panic!("stream yielded error: {e}"),
            }
        }

        // Collect all Part::FunctionCall parts
        let function_calls: Vec<_> = responses
            .iter()
            .filter_map(|r| r.content.as_ref())
            .flat_map(|c| &c.parts)
            .filter_map(|p| match p {
                Part::FunctionCall { name, args, id, .. } => {
                    Some((name.clone(), args.clone(), id.clone()))
                }
                _ => None,
            })
            .collect();

        // BUG CONDITION: on unfixed code, no SSE processing occurs, so no
        // tool calls are accumulated from streaming chunks.
        assert!(
            !function_calls.is_empty(),
            "expected Part::FunctionCall from accumulated delta.tool_calls chunks. \
             Counterexample: no Part::FunctionCall yielded — tool calls not streamed — bug confirmed."
        );

        // Verify the accumulated tool call
        let (name, args, id) = &function_calls[0];
        assert_eq!(name, "get_weather", "tool call name should be get_weather");
        assert_eq!(args["city"], "Paris", "tool call args should contain city=Paris");
        assert_eq!(id.as_deref(), Some("call_abc"), "tool call id should be call_abc");

        // The response containing the tool call should be the final one
        let final_resp = responses.last().unwrap();
        assert!(!final_resp.partial, "final response with tool calls should have partial=false");
        assert!(
            !final_resp.turn_complete,
            "final response carrying tool calls must have turn_complete=false — the turn \
             continues until tool results are processed (issue #401)"
        );
        let usage = final_resp
            .usage_metadata
            .as_ref()
            .expect("usage-only chunk should be attached to the tool-call response");
        assert_eq!(usage.prompt_token_count, 10);
        assert_eq!(usage.candidates_token_count, 15);
        assert_eq!(usage.total_token_count, 25);
    }

    #[tokio::test]
    async fn structured_tool_call_arguments_are_preserved() {
        let server = MockServer::start().await;

        let sse_body = [
            "data: {\"id\":\"chatcmpl-1\",\"object\":\"chat.completion.chunk\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"tool_calls\":[{\"index\":0,\"id\":\"call_abc\",\"type\":\"function\",\"function\":{\"name\":\"run_command\",\"arguments\":{\"command\":\"printf 'OK\\\\n'\"}}}]},\"finish_reason\":null}]}\n\n",
            "data: {\"id\":\"chatcmpl-1\",\"object\":\"chat.completion.chunk\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"tool_calls\"}],\"usage\":null}\n\n",
            "data: {\"id\":\"chatcmpl-1\",\"object\":\"chat.completion.chunk\",\"choices\":[],\"usage\":{\"prompt_tokens\":10,\"completion_tokens\":15,\"total_tokens\":25}}\n\n",
            "data: [DONE]\n\n",
        ]
        .join("");

        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_string(&sse_body)
                    .insert_header("content-type", "text/event-stream"),
            )
            .expect(1)
            .mount(&server)
            .await;

        let client = make_client(&server.uri());
        let mut stream = client
            .generate_content(make_request(), true)
            .await
            .expect("generate_content should not error");

        let mut function_call = None;
        while let Some(item) = stream.next().await {
            let response = item.expect("stream should not yield an error");
            function_call = response
                .content
                .as_ref()
                .into_iter()
                .flat_map(|content| &content.parts)
                .find_map(|part| match part {
                    Part::FunctionCall { name, args, id, .. } => {
                        Some((name.clone(), args.clone(), id.clone()))
                    }
                    _ => None,
                })
                .or(function_call);
        }

        let (name, args, id) = function_call.expect("expected a function call");
        assert_eq!(name, "run_command");
        assert_eq!(args["command"], "printf 'OK\\n'");
        assert_eq!(id.as_deref(), Some("call_abc"));
    }

    #[tokio::test]
    async fn repeated_structured_tool_call_snapshots_do_not_corrupt_arguments() {
        let server = MockServer::start().await;

        let sse_body = [
            "data: {\"id\":\"chatcmpl-1\",\"object\":\"chat.completion.chunk\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"tool_calls\":[{\"index\":0,\"id\":\"call_abc\",\"type\":\"function\",\"function\":{\"name\":\"run_command\",\"arguments\":{\"command\":\"printf 'OK\\\\n'\"}}}]},\"finish_reason\":null}]}\n\n",
            "data: {\"id\":\"chatcmpl-1\",\"object\":\"chat.completion.chunk\",\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":{}}}]},\"finish_reason\":null}]}\n\n",
            "data: {\"id\":\"chatcmpl-1\",\"object\":\"chat.completion.chunk\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"tool_calls\"}],\"usage\":null}\n\n",
            "data: [DONE]\n\n",
        ]
        .join("");

        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_string(&sse_body)
                    .insert_header("content-type", "text/event-stream"),
            )
            .expect(1)
            .mount(&server)
            .await;

        let client = make_client(&server.uri());
        let mut stream = client
            .generate_content(make_request(), true)
            .await
            .expect("generate_content should not error");

        let mut function_call = None;
        while let Some(item) = stream.next().await {
            let response = item.expect("stream should not yield an error");
            function_call = response
                .content
                .as_ref()
                .into_iter()
                .flat_map(|content| &content.parts)
                .find_map(|part| match part {
                    Part::FunctionCall { args, .. } => Some(args.clone()),
                    _ => None,
                })
                .or(function_call);
        }

        let args = function_call.expect("expected a function call");
        assert_eq!(args["command"], "printf 'OK\\n'");
    }

    /// Regression test for providers using "delta.reasoning" field instead of "delta.reasoning_content"
    /// (OpenRouter, Kilo Gateway, SambaNova, Cerebras, Groq).
    /// Without the fallback fix, this test would fail — reasoning silently dropped in streaming.
    #[tokio::test]
    async fn reasoning_field_fallback_streaming() {
        let server = MockServer::start().await;

        let sse_body = [
            "data: {\"id\":\"chatcmpl-1\",\"object\":\"chat.completion.chunk\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"reasoning\":\"Let me think...\"},\"finish_reason\":null}]}\n\n",
            "data: {\"id\":\"chatcmpl-1\",\"object\":\"chat.completion.chunk\",\"choices\":[{\"index\":0,\"delta\":{\"reasoning\":\" about this.\"},\"finish_reason\":null}]}\n\n",
            "data: {\"id\":\"chatcmpl-1\",\"object\":\"chat.completion.chunk\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"The answer is 42.\"},\"finish_reason\":null}]}\n\n",
            "data: {\"id\":\"chatcmpl-1\",\"object\":\"chat.completion.chunk\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":5,\"completion_tokens\":10,\"total_tokens\":15}}\n\n",
            "data: [DONE]\n\n",
        ]
        .join("");

        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_string(&sse_body)
                    .insert_header("content-type", "text/event-stream"),
            )
            .expect(1)
            .mount(&server)
            .await;

        let client = make_client(&server.uri());
        let request = make_request();

        let stream_result = client.generate_content(request, true).await;
        assert!(stream_result.is_ok(), "generate_content should not error");

        let mut stream = stream_result.unwrap();
        let mut responses = Vec::new();
        while let Some(item) = stream.next().await {
            match item {
                Ok(resp) => responses.push(resp),
                Err(e) => panic!("stream yielded error: {e}"),
            }
        }

        // Collect all Part::Thinking parts from all responses
        let thinking_parts: Vec<&str> = responses
            .iter()
            .filter_map(|r| r.content.as_ref())
            .flat_map(|c| &c.parts)
            .filter_map(|p| match p {
                Part::Thinking { thinking, .. } => Some(thinking.as_str()),
                _ => None,
            })
            .collect();

        // BUG CONDITION: on unfixed code, delta.reasoning is not parsed,
        // so no Part::Thinking is yielded from streaming delta chunks.
        assert!(
            !thinking_parts.is_empty(),
            "expected Part::Thinking responses from delta.reasoning chunks (fallback). \
             Counterexample: no Part::Thinking yielded — reasoning field not parsed — bug confirmed."
        );

        // Verify the thinking content matches what was sent
        let combined_thinking: String = thinking_parts.join("");
        assert!(
            combined_thinking.contains("Let me think"),
            "thinking content should contain the reasoning text"
        );
    }

    #[tokio::test]
    async fn response_observations_stream_keeps_first_id_and_final_trailer_once() {
        use serde_json::json;
        for tool in [false, true] {
            let server = MockServer::start().await;
            let delta = if tool {
                json!({"tool_calls": [{"index": 0, "id": "call-observed", "type": "function",
                "function": {"name": "synthetic_tool", "arguments": "{}"}}]})
            } else {
                json!({"content": "Hello"})
            };
            let frames = [
                json!({"id": "stream-observed", "prompt_token_ids": [0, 11], "prompt_text": "DO_NOT_RETAIN",
                    "choices": [{"index": 0, "delta": delta, "token_ids": [20], "finish_reason": null}]}),
                json!({"choices": [{"index": 0, "delta": {}, "token_ids": [21, 22],
                    "finish_reason": if tool {"tool_calls"} else {"stop"}}]}),
                json!({"choices": [], "usage": {"prompt_tokens": 2, "completion_tokens": 3, "total_tokens": 5,
                    "prompt_tokens_details": {"cached_tokens": 0}, "completion_tokens_details": {"reasoning_tokens": 1}},
                    "metrics": {"time_to_first_token_ms": 0, "generation_time_ms": 3.5,
                        "queue_time_ms": null, "mean_itl_ms": 1.75, "tokens_per_second": 9,
                        "private_control": "DO_NOT_RETAIN"}, "continue_turn": true}),
            ];
            let sse = frames.iter().map(|v| format!("data: {v}\n\n")).collect::<String>()
                + "data: [DONE]\n\n";
            Mock::given(method("POST"))
                .and(path("/chat/completions"))
                .respond_with(
                    ResponseTemplate::new(200)
                        .set_body_string(sse)
                        .insert_header("content-type", "text/event-stream"),
                )
                .expect(1)
                .mount(&server)
                .await;
            let mut request = make_request();
            request.config = Some(adk_core::GenerateContentConfig {
                extensions: [(
                    "openai".to_owned(),
                    json!({"cache_salt": "synthetic-salt", "return_token_ids": true}),
                )]
                .into_iter()
                .collect(),
                ..Default::default()
            });
            let responses = make_client(&server.uri())
                .generate_content(request, true)
                .await
                .unwrap()
                .collect::<Vec<_>>()
                .await
                .into_iter()
                .collect::<Result<Vec<_>, _>>()
                .unwrap();
            let finals: Vec<_> = responses.iter().filter(|r| r.finish_reason.is_some()).collect();
            assert_eq!(finals.len(), 1);
            let final_response = finals[0];
            assert!(!final_response.partial);
            assert_eq!(final_response.turn_complete, !tool);
            assert_eq!(
                final_response.provider_metadata,
                Some(json!({"openai": {
                    "id": "stream-observed", "prompt_token_ids": [0, 11],
                    "choices": [{"index": 0, "token_ids": [20]}, {"index": 0, "token_ids": [21, 22]}],
                    "metrics": {"time_to_first_token_ms": 0, "generation_time_ms": 3.5,
                        "queue_time_ms": null, "mean_itl_ms": 1.75, "tokens_per_second": 9}
                }}))
            );
            assert_eq!(
                final_response.usage_metadata.as_ref().unwrap().provider_usage,
                Some(frames[2]["usage"].clone())
            );
            assert!(responses.iter().filter(|r| r.partial).all(|r| r.provider_metadata.is_none()));
            if tool {
                assert!(
                    matches!(&final_response.content.as_ref().unwrap().parts[0], Part::FunctionCall { name, .. } if name == "synthetic_tool")
                );
            } else {
                assert_eq!(responses.len(), 2);
                assert!(
                    matches!(&responses[0].content.as_ref().unwrap().parts[0], Part::Text { text } if text == "Hello")
                );
            }
            let decoded: adk_core::LlmResponse =
                serde_json::from_value(serde_json::to_value(final_response).unwrap()).unwrap();
            assert_eq!(decoded.provider_metadata, final_response.provider_metadata);
            let requests = server.received_requests().await.unwrap();
            let wire: serde_json::Value = serde_json::from_slice(&requests[0].body).unwrap();
            assert_eq!(wire["cache_salt"], "synthetic-salt");
            assert_eq!(wire["return_token_ids"], true);
            assert_eq!(wire["stream_options"]["include_usage"], true);
        }
    }

    #[tokio::test]
    async fn response_observations_stream_presence_is_not_observed_zero() {
        use serde_json::json;
        let cases = [
            (json!({}), None),
            (
                json!({"id": null, "metrics": null, "prompt_token_ids": null}),
                Some(json!({"openai": {"id": null, "metrics": null, "prompt_token_ids": null}})),
            ),
            (
                json!({"id": false, "metrics": [], "prompt_token_ids": "invalid"}),
                Some(
                    json!({"openai": {"id": false, "metrics": [], "prompt_token_ids": "invalid"}}),
                ),
            ),
            (
                json!({"id": "", "metrics": {}, "prompt_token_ids": []}),
                Some(json!({"openai": {"id": "", "metrics": {}, "prompt_token_ids": []}})),
            ),
            (
                json!({"metrics": {"queue_time_ms": 0, "private_control": true}}),
                Some(json!({"openai": {"metrics": {"queue_time_ms": 0}}})),
            ),
        ];
        for (fields, expected) in cases {
            let server = MockServer::start().await;
            let finish = json!({"choices": [{"delta": {}, "finish_reason": "stop"}]});
            let mut trailer =
                json!({"choices": [], "prompt_text": "DO_NOT_RETAIN", "content_complete": true});
            trailer.as_object_mut().unwrap().extend(fields.as_object().unwrap().clone());
            let body = format!("data: {finish}\n\ndata: {trailer}\n\ndata: [DONE]\n\n");
            Mock::given(method("POST"))
                .respond_with(
                    ResponseTemplate::new(200)
                        .set_body_string(body)
                        .insert_header("content-type", "text/event-stream"),
                )
                .expect(1)
                .mount(&server)
                .await;
            let responses = make_client(&server.uri())
                .generate_content(make_request(), true)
                .await
                .unwrap()
                .collect::<Vec<_>>()
                .await;
            assert_eq!(responses.len(), 1);
            let response = responses[0].as_ref().unwrap();
            assert!(response.turn_complete && !response.partial);
            assert!(response.usage_metadata.is_none());
            assert_eq!(response.provider_metadata, expected);
        }
    }

    #[tokio::test]
    async fn response_observations_stream_token_record_presence() {
        use serde_json::json;
        for tokens in [json!(null), json!("invalid"), json!([]), json!([0])] {
            let server = MockServer::start().await;
            let finish = json!({"choices": [{"index": 7, "delta": {}, "finish_reason": "stop",
                "token_ids": tokens, "prompt_text": "DO_NOT_RETAIN"}]});
            let body = format!("data: {finish}\n\ndata: [DONE]\n\n");
            Mock::given(method("POST"))
                .respond_with(
                    ResponseTemplate::new(200)
                        .set_body_string(body)
                        .insert_header("content-type", "text/event-stream"),
                )
                .expect(1)
                .mount(&server)
                .await;
            let responses = make_client(&server.uri())
                .generate_content(make_request(), true)
                .await
                .unwrap()
                .collect::<Vec<_>>()
                .await;
            assert_eq!(responses.len(), 1);
            assert_eq!(
                responses[0].as_ref().unwrap().provider_metadata,
                Some(json!({"openai": {"choices": [{"index": 7, "token_ids": tokens}]}}))
            );
        }
    }

    #[tokio::test]
    async fn response_observations_retry_discards_failed_attempt_body() {
        use serde_json::json;
        use std::sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        };
        for streaming in [false, true] {
            let server = MockServer::start().await;
            let calls = Arc::new(AtomicUsize::new(0));
            let seen = calls.clone();
            Mock::given(method("POST")).respond_with(move |_: &wiremock::Request| {
                if seen.fetch_add(1, Ordering::SeqCst) == 0 {
                    ResponseTemplate::new(503).set_body_json(json!({"id": "failed-attempt",
                        "metrics": {"queue_time_ms": 999}, "prompt_token_ids": [999],
                        "choices": [{"index": 0, "token_ids": [999]}]}))
                } else if streaming {
                    ResponseTemplate::new(200).insert_header("content-type", "text/event-stream")
                        .set_body_string("data: {\"id\":\"accepted-attempt\",\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n")
                } else {
                    ResponseTemplate::new(200).set_body_json(json!({"id": "accepted-attempt",
                        "choices": [{"message": {"content": "ok"}, "finish_reason": "stop"}]}))
                }
            }).expect(2).mount(&server).await;
            let client = make_client(&server.uri()).with_retry_config(
                adk_model::retry::RetryConfig::default()
                    .with_max_retries(1)
                    .with_initial_delay(std::time::Duration::ZERO),
            );
            let responses = client
                .generate_content(make_request(), streaming)
                .await
                .unwrap()
                .collect::<Vec<_>>()
                .await;
            assert_eq!(responses.len(), 1);
            assert_eq!(
                responses[0].as_ref().unwrap().provider_metadata,
                Some(json!({"openai": {"id": "accepted-attempt"}}))
            );
            assert_eq!(calls.load(Ordering::SeqCst), 2);
        }
    }

    #[tokio::test]
    async fn response_observations_delivery_inline_text() {
        assert_observations_delivered_before_body_end(false, false, false).await;
    }

    #[tokio::test]
    async fn response_observations_delivery_inline_tool() {
        assert_observations_delivered_before_body_end(true, false, false).await;
    }

    #[tokio::test]
    async fn response_observations_delivery_trailer_text() {
        assert_observations_delivered_before_body_end(false, true, false).await;
    }

    #[tokio::test]
    async fn response_observations_delivery_trailer_tool() {
        assert_observations_delivered_before_body_end(true, true, false).await;
    }

    #[tokio::test]
    async fn response_observations_delivery_before_read_error_text() {
        assert_observations_delivered_before_body_end(false, false, true).await;
    }

    #[tokio::test]
    async fn response_observations_delivery_before_read_error_tool() {
        assert_observations_delivered_before_body_end(true, false, true).await;
    }

    async fn assert_observations_delivered_before_body_end(
        tool: bool,
        usage_trailer: bool,
        body_error: bool,
    ) {
        use serde_json::json;
        use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let (written, wait_written) = tokio::sync::oneshot::channel();
        let (release, wait_release) = tokio::sync::oneshot::channel();
        let usage = json!({"prompt_tokens": 1, "completion_tokens": 2, "total_tokens": 3});
        let server = async {
            let (socket, _) = listener.accept().await.unwrap();
            let mut socket = BufReader::new(socket);
            let mut line = String::new();
            socket.read_line(&mut line).await.unwrap();
            assert!(line.starts_with("POST /chat/completions "));
            let mut content_length = None;
            loop {
                line.clear();
                assert_ne!(socket.read_line(&mut line).await.unwrap(), 0);
                if line == "\r\n" {
                    break;
                }
                if let Some((name, value)) = line.split_once(':')
                    && name.eq_ignore_ascii_case("content-length")
                {
                    content_length = Some(value.trim().parse::<usize>().unwrap());
                }
            }
            let content_length = content_length.unwrap();
            assert!(content_length < 8192);
            socket.read_exact(&mut vec![0; content_length]).await.unwrap();
            let socket = socket.get_mut();
            let first = json!({"id": "delivery", "prompt_token_ids": [0], "choices": [
                {"index": 0, "delta": {"content": "prefix"}, "token_ids": [20]}]});
            let delta = if tool {
                json!({"tool_calls": [{"index": 0, "id": "call-delivery", "type": "function",
                    "function": {"name": "synthetic_tool", "arguments": "{}"}}]})
            } else {
                json!({"content": "done"})
            };
            let mut finish = json!({"choices": [{"index": 0, "delta": delta, "token_ids": [21],
                "finish_reason": if tool {"tool_calls"} else {"stop"}}]});
            let trailer = json!({"choices": [], "usage": usage, "metrics": {"queue_time_ms": 0}});
            if !usage_trailer {
                finish["usage"] = usage.clone();
                finish["metrics"] = trailer["metrics"].clone();
            }
            let mut body = format!("data: {first}\n\ndata: {finish}\n\n");
            if usage_trailer {
                body.push_str(&format!("data: {trailer}\n\n"));
            }
            let header = if body_error {
                format!("Content-Length: {}\r\n", body.len() + 100)
            } else {
                "Transfer-Encoding: chunked\r\n".to_owned()
            };
            socket.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\n{header}Connection: close\r\n\r\n").as_bytes()).await.unwrap();
            if !body_error {
                socket.write_all(format!("{:x}\r\n", body.len()).as_bytes()).await.unwrap();
            }
            socket.write_all(body.as_bytes()).await.unwrap();
            if !body_error {
                socket.write_all(b"\r\n").await.unwrap();
            }
            socket.flush().await.unwrap();
            written.send(()).unwrap();
            wait_release.await.unwrap();
            if !body_error {
                let tail = "data: {\"id\":\"after-final\",\"choices\":[],\"usage\":{\"total_tokens\":99},\"metrics\":{\"queue_time_ms\":99}}\n\ndata: [DONE]\n\n";
                socket
                    .write_all(format!("{:x}\r\n{tail}\r\n0\r\n\r\n", tail.len()).as_bytes())
                    .await
                    .unwrap();
            }
            socket.shutdown().await.unwrap();
        };
        let client = async {
            let mut stream = make_client(&format!("http://{address}"))
                .generate_content(make_request(), true)
                .await
                .unwrap();
            let first = stream.next().await.unwrap().unwrap();
            assert!(first.partial && first.provider_metadata.is_none());
            wait_written.await.unwrap();
            let final_response =
                tokio::time::timeout(std::time::Duration::from_secs(2), stream.next()).await;
            // Release and drain before asserting, including the expected RED path.
            release.send(()).unwrap();
            let tail: Vec<_> = stream.collect().await;
            let final_response = final_response
                .expect("final must arrive before the server releases the response body")
                .unwrap()
                .unwrap();
            assert!(!final_response.partial);
            assert_eq!(final_response.turn_complete, !tool);
            assert!(final_response.finish_reason.is_some());
            assert_eq!(final_response.content.as_ref().unwrap().has_function_calls(), tool);
            assert_eq!(final_response.usage_metadata.unwrap().provider_usage, Some(usage.clone()));
            assert_eq!(
                final_response.provider_metadata,
                Some(json!({"openai": {
                    "id": "delivery", "prompt_token_ids": [0], "metrics": {"queue_time_ms": 0},
                    "choices": [{"index": 0, "token_ids": [20]}, {"index": 0, "token_ids": [21]}]
                }}))
            );
            if body_error {
                assert_eq!(tail.len(), 1);
                assert!(tail[0].as_ref().unwrap_err().to_string().contains("stream read error"));
            } else {
                assert!(tail.is_empty(), "later metadata must not emit another final");
            }
        };
        // Both futures own their sockets; timeout or panic drops them without detached tasks.
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            tokio::join!(server, client)
        })
        .await
        .expect("bounded delivery fixture completes");
    }

    #[tokio::test]
    async fn response_observations_fragmented_and_interrupted_sse() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        // 0: complete; 1: truncated trailer; 2: no finish; 3: HTTP body read error.
        for case in 0..4 {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = listener.local_addr().unwrap();
            let server = async {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut request = [0; 8192];
                let read = socket.read(&mut request).await.unwrap();
                assert!(request[..read].starts_with(b"POST /chat/completions "));
                let mut body = String::from(
                    "data: {\"id\":\"fragmented\",\"choices\":[{\"delta\":{\"content\":\"hi 🌍\"},\"token_ids\":[20]}]}\n\n",
                );
                if case != 2 {
                    body.push_str(
                        "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
                    );
                }
                if case == 0 {
                    body.push_str("data: {\"choices\":[],\"usage\":{\"total_tokens\":1},\"metrics\":{\"queue_time_ms\":0}}\n\ndata: [DONE]\n\n");
                } else {
                    body.push_str(
                        "data: {\"choices\":[],\"usage\":{\"total_tokens\":1},\"metrics\":",
                    );
                }
                let header = if case == 3 {
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        body.len() + 100
                    )
                } else {
                    "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n".to_owned()
                };
                socket.write_all(header.as_bytes()).await.unwrap();
                for bytes in body.as_bytes().chunks(3) {
                    if case != 3 {
                        socket
                            .write_all(format!("{:x}\r\n", bytes.len()).as_bytes())
                            .await
                            .unwrap();
                    }
                    socket.write_all(bytes).await.unwrap();
                    if case != 3 {
                        socket.write_all(b"\r\n").await.unwrap();
                    }
                    tokio::task::yield_now().await;
                }
                if case != 3 {
                    socket.write_all(b"0\r\n\r\n").await.unwrap();
                }
                socket.shutdown().await.unwrap();
            };
            let client = async {
                make_client(&format!("http://{address}"))
                    .generate_content(make_request(), true)
                    .await
                    .unwrap()
                    .collect::<Vec<_>>()
                    .await
            };
            let (_, responses) = tokio::time::timeout(std::time::Duration::from_secs(5), async {
                tokio::join!(server, client)
            })
            .await
            .expect("bounded loopback fixture completes");
            assert!(matches!(&responses[0].as_ref().unwrap().content.as_ref().unwrap().parts[0],
                Part::Text { text } if text == "hi 🌍"));
            let finals: Vec<_> = responses
                .iter()
                .filter_map(|r| r.as_ref().ok())
                .filter(|r| r.finish_reason.is_some())
                .collect();
            if case >= 2 {
                assert!(finals.is_empty());
                assert!(
                    responses
                        .iter()
                        .filter_map(|r| r.as_ref().ok())
                        .all(|r| r.provider_metadata.is_none() && r.usage_metadata.is_none())
                );
                assert_eq!(responses.iter().filter(|r| r.is_err()).count(), usize::from(case == 3));
            } else {
                assert_eq!(finals.len(), 1);
                let metadata = finals[0].provider_metadata.as_ref().unwrap();
                assert_eq!(metadata["openai"]["id"], "fragmented");
                assert_eq!(
                    metadata["openai"]["choices"],
                    serde_json::json!([{ "token_ids": [20] }])
                );
                if case == 0 {
                    assert_eq!(metadata["openai"]["metrics"]["queue_time_ms"], 0);
                    assert_eq!(
                        finals[0].usage_metadata.as_ref().unwrap().provider_usage,
                        Some(serde_json::json!({"total_tokens": 1}))
                    );
                } else {
                    assert!(metadata["openai"].get("metrics").is_none());
                    assert!(finals[0].usage_metadata.is_none());
                }
            }
        }
    }
}
