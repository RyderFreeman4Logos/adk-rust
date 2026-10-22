//! End-to-end validation of the ACP v1 server-direction features through a
//! real [`Runner`](adk_runner::Runner)-backed `AcpServer`, driven by the
//! official SDK `Client` over an in-process [`Channel::duplex`] — no subprocess
//! and no API key.
//!
//! Each test pairs the SDK `Client` with the ADK server wiring built from the
//! public `AcpSessionHandler`, exactly as an IDE would over stdio, and asserts:
//!
//! - embedded-resource prompts reach the agent as `Part::EmbeddedResource`;
//! - image / audio prompts are accepted and reach the agent as
//!   `Part::InlineData`;
//! - the permission bridge fires for a confirmation-gated tool (allow executes,
//!   deny skips), and the outer prompt completes in both cases;
//! - `session/load` replays stored updates in chronological order;
//! - `UsageUpdate` and an enriched `ToolCallUpdate` are surfaced.

use std::sync::{Arc, Mutex};

use acp_full_protocol::{
    AGENT_NAME, CONFIRM_CALL_ID, ScriptedAgent, build_delete_tool, build_session_service,
};
use adk_acp::server::{
    AcpServerConfigBuilder, AcpServerError, AcpSessionHandler, AgentCapabilities,
    CapabilitiesBuilder, ResponseStreamer,
};
use adk_core::Agent as AdkAgent;
use adk_session::{GetRequest, SessionService};
use agent_client_protocol::schema::ProtocolVersion;
use agent_client_protocol::schema::v1::{
    AudioContent, CloseSessionRequest, CloseSessionResponse, ContentBlock,
    EmbeddedResource as AcpEmbeddedResource, EmbeddedResourceResource, ImageContent,
    Implementation, InitializeRequest, InitializeResponse, LoadSessionRequest, LoadSessionResponse,
    NewSessionRequest, NewSessionResponse, PermissionOptionKind, PromptRequest, PromptResponse,
    RequestPermissionOutcome, RequestPermissionRequest, RequestPermissionResponse,
    SelectedPermissionOutcome, SessionNotification, SessionUpdate, StopReason, TextContent,
    TextResourceContents as AcpTextResourceContents,
};
use agent_client_protocol::{Agent, Channel, Client, ConnectTo, ConnectionTo, Error, Responder};
use base64::{Engine as _, engine::general_purpose};
use tokio_util::sync::CancellationToken;

// ─────────────────────────────────────────────────────────────────────────────
// Server wiring — mirrors the crate-internal stdio transport, built entirely on
// the public `AcpSessionHandler` API so it can serve over any SDK channel.
// ─────────────────────────────────────────────────────────────────────────────

async fn serve_over_channel<C>(
    handler: Arc<AcpSessionHandler>,
    capabilities: AgentCapabilities,
    name: String,
    title: String,
    component: C,
) -> Result<(), Error>
where
    C: ConnectTo<Agent> + 'static,
{
    let new_handler = handler.clone();
    let prompt_handler = handler.clone();
    let load_handler = handler.clone();
    let close_handler = handler;

    Agent
        .builder()
        .name(name.clone())
        .on_receive_request(
            move |request: InitializeRequest,
                  responder: Responder<InitializeResponse>,
                  _connection: ConnectionTo<Client>| {
                let capabilities = capabilities.clone();
                let name = name.clone();
                let title = title.clone();
                async move {
                    let _ = request;
                    let mut implementation = Implementation::new(name, env!("CARGO_PKG_VERSION"));
                    if !title.is_empty() {
                        implementation = implementation.title(title);
                    }
                    responder.respond(
                        InitializeResponse::new(ProtocolVersion::V1)
                            .agent_capabilities(capabilities)
                            .agent_info(implementation),
                    )
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            move |request: NewSessionRequest,
                  responder: Responder<NewSessionResponse>,
                  connection: ConnectionTo<Client>| {
                let handler = new_handler.clone();
                async move {
                    let cancellation = responder.cancellation();
                    connection.spawn(async move {
                        responder.respond_with_result(
                            handler
                                .create_session(request, cancellation)
                                .await
                                .map(NewSessionResponse::new)
                                .map_err(to_protocol_error),
                        )
                    })?;
                    Ok(())
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            move |request: PromptRequest,
                  responder: Responder<PromptResponse>,
                  connection: ConnectionTo<Client>| {
                let handler = prompt_handler.clone();
                async move {
                    let cancellation = responder.cancellation();
                    let spawned_connection = connection.clone();
                    connection.spawn(async move {
                        let session_id = request.session_id.clone();
                        let cancellation_handler = handler.clone();
                        let work = handler.handle_prompt(request, spawned_connection);
                        tokio::pin!(work);
                        let result = tokio::select! {
                            result = &mut work => result,
                            _ = cancellation.cancelled() => {
                                cancellation_handler.cancel_session(&session_id).await;
                                work.await
                            }
                        };
                        responder.respond_with_result(
                            result.map(PromptResponse::new).map_err(to_protocol_error),
                        )
                    })?;
                    Ok(())
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            move |request: LoadSessionRequest,
                  responder: Responder<LoadSessionResponse>,
                  connection: ConnectionTo<Client>| {
                let handler = load_handler.clone();
                async move {
                    let cancellation = responder.cancellation();
                    let spawned_connection = connection.clone();
                    connection.spawn(async move {
                        responder.respond_with_result(
                            handler
                                .load_session(
                                    &request.session_id,
                                    request.cwd,
                                    request.additional_directories,
                                    request.mcp_servers,
                                    cancellation,
                                    spawned_connection,
                                )
                                .await
                                .map(|()| LoadSessionResponse::new())
                                .map_err(to_protocol_error),
                        )
                    })?;
                    Ok(())
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async move |request: CloseSessionRequest,
                        responder: Responder<CloseSessionResponse>,
                        _connection: ConnectionTo<Client>| {
                match close_handler.close_session(&request.session_id).await {
                    Ok(()) => responder.respond(CloseSessionResponse::new()),
                    Err(error) => responder.respond_with_error(to_protocol_error(error)),
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .connect_to(component)
        .await
}

fn to_protocol_error(error: AcpServerError) -> Error {
    match error {
        AcpServerError::MalformedMessage(message)
        | AcpServerError::SessionNotFound(message)
        | AcpServerError::UnsupportedVersion { requested: message, .. } => {
            Error::invalid_params().data(message)
        }
        AcpServerError::MaxSessionsReached(max) => {
            Error::invalid_params().data(format!("maximum active sessions reached: {max}"))
        }
        other => Error::internal_error().data(other.to_string()),
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Helpers
// ─────────────────────────────────────────────────────────────────────────────

/// How the test client answers a `session/request_permission` request.
#[derive(Clone, Copy)]
enum PermissionAnswer {
    Allow,
    Deny,
    Cancel,
}

/// Result of driving one prompt turn end to end.
struct DriveResult {
    stop_reason: StopReason,
    updates: Vec<SessionUpdate>,
    permission_call_id: Option<String>,
}

/// Text of an `AgentMessageChunk`, if the update is one carrying text.
fn message_chunk_text(update: &SessionUpdate) -> Option<String> {
    match update {
        SessionUpdate::AgentMessageChunk(chunk) => match &chunk.content {
            ContentBlock::Text(text) => Some(text.text.clone()),
            _ => None,
        },
        _ => None,
    }
}

/// Build server config, handler, and capabilities for the given agent.
fn build_server(
    agent: Arc<dyn AdkAgent>,
    session_service: Arc<dyn SessionService>,
) -> (Arc<AcpSessionHandler>, AgentCapabilities) {
    let config = AcpServerConfigBuilder::new()
        .agent(agent)
        .session_service(session_service)
        .agent_name(AGENT_NAME)
        .agent_description("ACP full-protocol reference agent")
        .build()
        .expect("valid config");
    let capabilities = CapabilitiesBuilder::build(&config);
    let handler =
        Arc::new(AcpSessionHandler::new(&config, CancellationToken::new()).expect("handler"));
    (handler, capabilities)
}

/// Drive `initialize` → `session/new` → `session/prompt` against the server,
/// answering any `session/request_permission` with `answer`, and capture every
/// `session/update` plus the stop reason.
async fn drive_prompt(
    agent: Arc<dyn AdkAgent>,
    session_service: Arc<dyn SessionService>,
    prompt: Vec<ContentBlock>,
    answer: PermissionAnswer,
) -> DriveResult {
    let (handler, capabilities) = build_server(agent, session_service);
    let updates = Arc::new(Mutex::new(Vec::new()));
    let updates_for_client = updates.clone();
    let permission_call_id = Arc::new(Mutex::new(None::<String>));
    let permission_for_client = permission_call_id.clone();
    let (server_channel, client_channel) = Channel::duplex();

    let server = serve_over_channel(
        handler,
        capabilities,
        AGENT_NAME.into(),
        "ACP full-protocol reference agent".into(),
        server_channel,
    );

    let client = Client
        .builder()
        .on_receive_notification(
            async move |notification: SessionNotification, _connection: ConnectionTo<Agent>| {
                updates_for_client.lock().expect("updates lock").push(notification.update);
                Ok(())
            },
            agent_client_protocol::on_receive_notification!(),
        )
        .on_receive_request(
            move |request: RequestPermissionRequest,
                  responder: Responder<RequestPermissionResponse>,
                  _connection: ConnectionTo<Agent>| {
                let captured = permission_for_client.clone();
                async move {
                    *captured.lock().expect("permission lock") =
                        Some(request.tool_call.tool_call_id.to_string());
                    let outcome = match answer {
                        PermissionAnswer::Allow => {
                            let option = request
                                .options
                                .iter()
                                .find(|option| option.kind == PermissionOptionKind::AllowOnce)
                                .expect("allow option offered");
                            RequestPermissionOutcome::Selected(SelectedPermissionOutcome::new(
                                option.option_id.clone(),
                            ))
                        }
                        PermissionAnswer::Deny => {
                            let option = request
                                .options
                                .iter()
                                .find(|option| option.kind == PermissionOptionKind::RejectOnce)
                                .expect("reject option offered");
                            RequestPermissionOutcome::Selected(SelectedPermissionOutcome::new(
                                option.option_id.clone(),
                            ))
                        }
                        PermissionAnswer::Cancel => RequestPermissionOutcome::Cancelled,
                    };
                    responder.respond(RequestPermissionResponse::new(outcome))
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .connect_with(client_channel, move |connection: ConnectionTo<Agent>| async move {
            connection
                .send_request(InitializeRequest::new(ProtocolVersion::V1))
                .block_task()
                .await?;
            let cwd = std::env::current_dir().expect("absolute cwd");
            let session = connection.send_request(NewSessionRequest::new(cwd)).block_task().await?;
            let prompt = connection
                .send_request(PromptRequest::new(session.session_id.clone(), prompt))
                .block_task()
                .await?;
            Ok(prompt.stop_reason)
        });

    let server_task = tokio::spawn(server);
    let stop_reason = tokio::time::timeout(std::time::Duration::from_secs(5), client)
        .await
        .expect("prompt completed before timeout")
        .expect("official ACP client completed");
    server_task.abort();
    let _ = server_task.await;

    let updates = updates.lock().expect("updates lock").clone();
    let permission_call_id = permission_call_id.lock().expect("permission lock").clone();
    DriveResult { stop_reason, updates, permission_call_id }
}

fn text_block(text: &str) -> ContentBlock {
    ContentBlock::Text(TextContent::new(text))
}

fn embedded_text_block(uri: &str, text: &str) -> ContentBlock {
    ContentBlock::Resource(AcpEmbeddedResource::new(
        EmbeddedResourceResource::TextResourceContents(
            AcpTextResourceContents::new(text, uri).mime_type(Some("text/markdown".to_string())),
        ),
    ))
}

fn image_block(bytes: &[u8]) -> ContentBlock {
    ContentBlock::Image(ImageContent::new(general_purpose::STANDARD.encode(bytes), "image/png"))
}

fn audio_block(bytes: &[u8]) -> ContentBlock {
    ContentBlock::Audio(AudioContent::new(general_purpose::STANDARD.encode(bytes), "audio/wav"))
}

// ─────────────────────────────────────────────────────────────────────────────
// Tests
// ─────────────────────────────────────────────────────────────────────────────

/// Embedded-resource prompt round-trip **and** usage / tool-call surfacing.
///
/// Sending a `session/prompt` carrying an `EmbeddedResource` block makes the
/// agent echo the resource's uri and text — which is only possible if the block
/// reached the agent as `Part::EmbeddedResource`. The same rich turn emits a
/// `UsageUpdate` and an enriched `ToolCallUpdate`.
#[tokio::test]
async fn embedded_resource_prompt_round_trips_and_surfaces_usage_and_tool_update() {
    let session_service = build_session_service();
    let agent: Arc<dyn AdkAgent> = Arc::new(ScriptedAgent::new(build_delete_tool()));
    let prompt = vec![text_block("inspect"), embedded_text_block("file:///notes.md", "# Notes")];

    let result = drive_prompt(agent, session_service, prompt, PermissionAnswer::Cancel).await;
    assert_eq!(result.stop_reason, StopReason::EndTurn);

    // The embedded resource reached the agent (echoed uri + text).
    let echoed: Vec<String> = result.updates.iter().filter_map(message_chunk_text).collect();
    assert!(
        echoed.iter().any(|text| text == "embedded-resource:file:///notes.md:# Notes"),
        "embedded resource must reach the agent as Part::EmbeddedResource, got {echoed:?}"
    );

    // A single UsageUpdate reflects the reported token count.
    let usage: Vec<_> = result
        .updates
        .iter()
        .filter_map(|update| match update {
            SessionUpdate::UsageUpdate(usage) => Some(usage.used),
            _ => None,
        })
        .collect();
    assert_eq!(usage, vec![50], "exactly one UsageUpdate with the reported total tokens");

    // An enriched ToolCallUpdate surfaces the tool result with a file location.
    let has_enriched_tool_update = result.updates.iter().any(|update| match update {
        SessionUpdate::ToolCallUpdate(update) => {
            update.fields.locations.as_ref().is_some_and(|locations| !locations.is_empty())
        }
        _ => false,
    });
    assert!(has_enriched_tool_update, "an enriched ToolCallUpdate with locations must be emitted");
}

/// Multimodal prompt acceptance: image and audio blocks are accepted and reach
/// the agent as `Part::InlineData` (echoed with mime type and byte length).
#[tokio::test]
async fn multimodal_prompt_is_accepted_as_inline_data() {
    let session_service = build_session_service();
    let agent: Arc<dyn AdkAgent> = Arc::new(ScriptedAgent::new(build_delete_tool()));
    let prompt = vec![text_block("look"), image_block(&[1, 2, 3, 4]), audio_block(&[5, 6, 7])];

    let result = drive_prompt(agent, session_service, prompt, PermissionAnswer::Cancel).await;
    assert_eq!(result.stop_reason, StopReason::EndTurn, "multimodal prompt must be accepted");

    let echoed: Vec<String> = result.updates.iter().filter_map(message_chunk_text).collect();
    assert!(
        echoed.iter().any(|text| text == "inline-data:image/png:4"),
        "image must reach the agent as Part::InlineData, got {echoed:?}"
    );
    assert!(
        echoed.iter().any(|text| text == "inline-data:audio/wav:3"),
        "audio must reach the agent as Part::InlineData, got {echoed:?}"
    );
}

/// Permission bridge — approval executes the gated tool, and the outer prompt
/// still completes after the nested `session/request_permission` round trip.
#[tokio::test]
async fn permission_allow_executes_gated_tool_and_completes_turn() {
    let session_service = build_session_service();
    let scripted = ScriptedAgent::new(build_delete_tool());
    let executed = scripted.executed_flag();
    let agent: Arc<dyn AdkAgent> = Arc::new(scripted);

    let result = drive_prompt(
        agent,
        session_service,
        vec![text_block("delete the report")],
        PermissionAnswer::Allow,
    )
    .await;

    assert_eq!(result.stop_reason, StopReason::EndTurn, "outer prompt must complete");
    assert!(*executed.lock().expect("executed lock"), "tool must execute on allow");
    assert_eq!(
        result.permission_call_id.as_deref(),
        Some(CONFIRM_CALL_ID),
        "permission request must correlate to the paused tool call"
    );
    let echoed: Vec<String> = result.updates.iter().filter_map(message_chunk_text).collect();
    assert!(
        echoed.iter().any(|text| text.contains("tool executed")),
        "executed message must be streamed, got {echoed:?}"
    );
}

/// Permission bridge — denial skips the gated tool, and the outer prompt still
/// completes.
#[tokio::test]
async fn permission_deny_skips_gated_tool_and_completes_turn() {
    let session_service = build_session_service();
    let scripted = ScriptedAgent::new(build_delete_tool());
    let executed = scripted.executed_flag();
    let agent: Arc<dyn AdkAgent> = Arc::new(scripted);

    let result = drive_prompt(
        agent,
        session_service,
        vec![text_block("delete the report")],
        PermissionAnswer::Deny,
    )
    .await;

    assert_eq!(result.stop_reason, StopReason::EndTurn, "outer prompt must complete");
    assert!(!*executed.lock().expect("executed lock"), "tool must not execute on deny");
    assert_eq!(result.permission_call_id.as_deref(), Some(CONFIRM_CALL_ID));
    let echoed: Vec<String> = result.updates.iter().filter_map(message_chunk_text).collect();
    assert!(
        echoed.iter().any(|text| text.contains("skipped")),
        "skipped message must be streamed, got {echoed:?}"
    );
}

/// `session/load` replays the stored conversation in chronological order before
/// the load request completes.
#[tokio::test]
async fn session_load_replays_history_in_chronological_order() {
    let session_service = build_session_service();
    let session_service_probe = session_service.clone();
    let agent: Arc<dyn AdkAgent> = Arc::new(ScriptedAgent::new(build_delete_tool()));
    let (handler, capabilities) = build_server(agent, session_service);

    let updates = Arc::new(Mutex::new(Vec::new()));
    let updates_for_client = updates.clone();
    let (server_channel, client_channel) = Channel::duplex();

    let server = serve_over_channel(
        handler,
        capabilities,
        AGENT_NAME.into(),
        "ACP full-protocol reference agent".into(),
        server_channel,
    );

    let client = Client
        .builder()
        .on_receive_notification(
            async move |notification: SessionNotification, _connection: ConnectionTo<Agent>| {
                updates_for_client.lock().expect("updates lock").push(notification.update);
                Ok(())
            },
            agent_client_protocol::on_receive_notification!(),
        )
        .connect_with(client_channel, move |connection: ConnectionTo<Agent>| {
            let updates = updates.clone();
            let session_service_probe = session_service_probe.clone();
            async move {
                let initialized = connection
                    .send_request(InitializeRequest::new(ProtocolVersion::V1))
                    .block_task()
                    .await?;
                assert!(
                    initialized.agent_capabilities.load_session,
                    "load_session capability must be advertised"
                );

                let cwd = std::env::current_dir().expect("absolute cwd");
                let session = connection
                    .send_request(NewSessionRequest::new(cwd.clone()))
                    .block_task()
                    .await?;
                let prompt = connection
                    .send_request(PromptRequest::new(
                        session.session_id.clone(),
                        vec![text_block("inspect"), embedded_text_block("file:///a.md", "# A")],
                    ))
                    .block_task()
                    .await?;
                assert_eq!(prompt.stop_reason, StopReason::EndTurn);

                // Expected replay order derived directly from the stored events.
                let persisted = session_service_probe
                    .get(GetRequest {
                        app_name: AGENT_NAME.to_string(),
                        user_id: "acp-client".to_string(),
                        session_id: session.session_id.to_string(),
                        num_recent_events: None,
                        after: None,
                    })
                    .await
                    .expect("persisted session");
                let expected: Vec<String> = persisted
                    .events()
                    .all()
                    .iter()
                    .flat_map(ResponseStreamer::map_event)
                    .filter_map(|update| message_chunk_text(&update))
                    .collect();
                assert!(
                    expected.iter().any(|text| text == "turn complete"),
                    "stored history must contain the agent's response, got {expected:?}"
                );

                // Drop the active session and clear captured prompt-turn updates,
                // so only replay updates remain.
                connection
                    .send_request(CloseSessionRequest::new(session.session_id.clone()))
                    .block_task()
                    .await?;
                updates.lock().expect("updates lock").clear();

                connection
                    .send_request(LoadSessionRequest::new(session.session_id.clone(), cwd.clone()))
                    .block_task()
                    .await?;

                let replayed: Vec<String> = updates
                    .lock()
                    .expect("updates lock")
                    .iter()
                    .filter_map(message_chunk_text)
                    .collect();
                assert_eq!(replayed, expected, "replay must match stored chronological order");
                Ok(())
            }
        });

    let server_task = tokio::spawn(server);
    tokio::time::timeout(std::time::Duration::from_secs(5), client)
        .await
        .expect("session load completed before timeout")
        .expect("official ACP client completed");
    server_task.abort();
    let _ = server_task.await;
}
