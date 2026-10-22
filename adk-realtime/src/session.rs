//! Core RealtimeSession trait definition.

use crate::audio::AudioChunk;
use crate::error::Result;
use crate::events::{ClientEvent, ServerEvent, ToolResponse};
use async_trait::async_trait;
use futures::Stream;
use std::pin::Pin;

/// The outcome of an attempt to mutate the session context mid-flight.
#[derive(Debug, Clone)]
pub enum ContextMutationOutcome {
    /// Provider successfully updated the active session via sideband.
    Applied,
    /// Provider requires the transport to be rebound with a new configuration.
    RequiresResumption(Box<crate::config::RealtimeConfig>),
}

/// A real-time bidirectional streaming session.
///
/// This trait provides a unified interface for real-time voice/audio sessions
/// across different providers (OpenAI, Gemini, etc.).
///
/// # Example
///
/// ```rust,ignore
/// use adk_realtime::{RealtimeSession, ServerEvent};
///
/// async fn handle_session(session: &dyn RealtimeSession) -> Result<()> {
///     // Send audio
///     session.send_audio(audio_chunk).await?;
///
///     // Receive events
///     while let Some(event) = session.next_event().await {
///         match event? {
///             ServerEvent::AudioDelta { delta, .. } => { /* play audio */ }
///             ServerEvent::FunctionCallDone { name, arguments, call_id, .. } => {
///                 // Execute tool and respond
///                 let result = execute_tool(&name, &arguments);
///                 session.send_tool_response(call_id, result).await?;
///             }
///             _ => {}
///         }
///     }
///     Ok(())
/// }
/// ```
/// Why a provider closed the stream, when it said.
///
/// A closed transport and a provider that deliberately hung up both reach a
/// polling caller as `next_event() -> None`, so without this they produce the
/// same terminal record. Distinguishing them matters: "the provider aborted an
/// idle session" and "the network dropped" call for different responses, and
/// an application that records the first as a generic stream failure sends an
/// operator looking for a defect that is not there.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DisconnectReason {
    /// The WebSocket close code, if the peer sent a close frame.
    pub code: Option<u16>,
    /// The peer's stated reason. Provider text, so treat it as data.
    pub reason: String,
}

impl std::fmt::Display for DisconnectReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.code {
            Some(code) if !self.reason.is_empty() => write!(f, "{code}:{}", self.reason),
            Some(code) => write!(f, "{code}"),
            None if !self.reason.is_empty() => write!(f, "{}", self.reason),
            None => write!(f, "unknown"),
        }
    }
}

#[async_trait]
pub trait RealtimeSession: Send + Sync {
    /// Get the session ID.
    fn session_id(&self) -> &str;

    /// Check if the session is currently connected.
    fn is_connected(&self) -> bool;

    /// Why the stream ended, if the provider said and the session recorded it.
    ///
    /// Defaulted to `None` so existing sessions keep compiling; a provider that
    /// sees a close frame should override it. Only meaningful after the event
    /// stream has ended — before that it reports whatever the last close was,
    /// which for a live session is nothing.
    fn disconnect_reason(&self) -> Option<DisconnectReason> {
        None
    }

    /// Send raw audio data to the server.
    ///
    /// The audio should be in the format specified in the session configuration.
    async fn send_audio(&self, audio: &AudioChunk) -> Result<()>;

    /// Send base64-encoded audio directly.
    async fn send_audio_base64(&self, audio_base64: &str) -> Result<()>;

    /// Send a text message.
    async fn send_text(&self, text: &str) -> Result<()>;

    /// Send a single video/image frame (base64-encoded, e.g. a JPEG) to the
    /// model for multimodal input. `mime_type` is the frame's media type
    /// (e.g. `image/jpeg`). The default is a no-op for providers/sessions that
    /// don't accept visual input.
    async fn send_video_frame(&self, _mime_type: &str, _data_base64: &str) -> Result<()> {
        Ok(())
    }

    /// Send a tool/function response (output **and** a response trigger).
    async fn send_tool_response(&self, response: ToolResponse) -> Result<()>;

    /// Send a tool/function output **without** triggering a response.
    ///
    /// When one model response dispatches several parallel tool calls, send each
    /// output with this method and then call
    /// [`create_response`](Self::create_response) exactly once — issuing a
    /// `response.create` per output would collide with the still-active response
    /// on providers like OpenAI. The default delegates to
    /// [`send_tool_response`](Self::send_tool_response) for backends that do not
    /// separate the two.
    async fn send_tool_output(&self, response: ToolResponse) -> Result<()> {
        self.send_tool_response(response).await
    }

    /// Commit the audio buffer (for manual VAD mode).
    async fn commit_audio(&self) -> Result<()>;

    /// Clear the audio input buffer.
    async fn clear_audio(&self) -> Result<()>;

    /// Trigger a response from the model.
    async fn create_response(&self) -> Result<()>;

    /// Interrupt/cancel the current response.
    async fn interrupt(&self) -> Result<()>;

    /// Send a raw client event.
    async fn send_event(&self, event: ClientEvent) -> Result<()>;

    /// Get the next event from the server.
    ///
    /// Returns `None` when the session is closed.
    async fn next_event(&self) -> Option<Result<ServerEvent>>;

    /// Get a stream of server events.
    fn events(&self) -> Pin<Box<dyn Stream<Item = Result<ServerEvent>> + Send + '_>>;

    /// Close the session gracefully.
    async fn close(&self) -> Result<()>;

    /// Attempt to mutate the session parameters mid-flight.
    ///
    /// For providers that support native hot-swapping (e.g., OpenAI), this
    /// mutates the parameters without tearing down the connection and returns `Ok(ContextMutationOutcome::Applied)`.
    /// For providers that require a static configuration (e.g., Gemini), this
    /// returns `Ok(ContextMutationOutcome::RequiresResumption(config))` to signal
    /// the runner to queue a session reconnect or resumption safely.
    async fn mutate_context(
        &self,
        config: crate::config::RealtimeConfig,
    ) -> Result<ContextMutationOutcome>;
}

/// Extension trait for RealtimeSession with convenience methods.
#[async_trait]
pub trait RealtimeSessionExt: RealtimeSession {
    /// Send audio and wait for the response to complete.
    async fn send_audio_and_wait(&self, audio: &AudioChunk) -> Result<Vec<ServerEvent>> {
        self.send_audio(audio).await?;
        self.commit_audio().await?;

        let mut events = Vec::new();
        while let Some(event) = self.next_event().await {
            let event = event?;
            let is_done = matches!(&event, ServerEvent::ResponseDone { .. });
            events.push(event);
            if is_done {
                break;
            }
        }
        Ok(events)
    }

    /// Send text and wait for the response to complete.
    async fn send_text_and_wait(&self, text: &str) -> Result<Vec<ServerEvent>> {
        self.send_text(text).await?;
        self.create_response().await?;

        let mut events = Vec::new();
        while let Some(event) = self.next_event().await {
            let event = event?;
            let is_done = matches!(&event, ServerEvent::ResponseDone { .. });
            events.push(event);
            if is_done {
                break;
            }
        }
        Ok(events)
    }

    /// Collect all audio chunks from a response (as raw bytes).
    async fn collect_audio(&self) -> Result<Vec<Vec<u8>>> {
        let mut audio_chunks = Vec::new();
        while let Some(event) = self.next_event().await {
            match event? {
                ServerEvent::AudioDelta { delta, .. } => {
                    audio_chunks.push(delta);
                }
                ServerEvent::ResponseDone { .. } => break,
                ServerEvent::Error { error, .. } => {
                    return Err(crate::error::RealtimeError::server(
                        error.code.unwrap_or_default(),
                        error.message,
                    ));
                }
                _ => {}
            }
        }
        Ok(audio_chunks)
    }
}

// Blanket implementation
impl<T: RealtimeSession> RealtimeSessionExt for T {}

/// A boxed session type for dynamic dispatch.
pub type BoxedSession = Box<dyn RealtimeSession>;

#[cfg(test)]
mod disconnect_reason_tests {
    use super::DisconnectReason;

    /// The string is what ends up in an application's terminal record, so the
    /// shape is a contract, not a debug convenience.
    #[test]
    fn a_code_and_reason_render_together() {
        let reason =
            DisconnectReason { code: Some(1008), reason: "The operation was aborted.".to_string() };

        assert_eq!(reason.to_string(), "1008:The operation was aborted.");
    }

    /// Providers do not always send both halves, and a close with only a code
    /// still carries the distinction the caller needs.
    #[test]
    fn either_half_alone_still_says_something() {
        assert_eq!(
            DisconnectReason { code: Some(1011), reason: String::new() }.to_string(),
            "1011"
        );
        assert_eq!(
            DisconnectReason { code: None, reason: "going away".to_string() }.to_string(),
            "going away"
        );
    }

    /// A transport that simply died sends no close frame at all. Rendering that
    /// as an empty string would produce terminal records ending in a bare
    /// separator, which reads as truncation rather than absence.
    #[test]
    fn an_absent_close_frame_is_named_rather_than_blank() {
        assert_eq!(DisconnectReason { code: None, reason: String::new() }.to_string(), "unknown");
    }
}
