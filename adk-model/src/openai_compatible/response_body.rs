//! Bounded body accumulation before non-streaming response parsing.

use adk_core::{AdkError, ErrorCategory, ErrorComponent};
use std::num::NonZeroUsize;

pub(super) async fn bounded(
    mut response: reqwest::Response,
    limit: NonZeroUsize,
    provider: &str,
    read_error_code: &'static str,
) -> Result<reqwest::Response, AdkError> {
    let status = response.status().as_u16();
    let overflow = || {
        AdkError::new(
            ErrorComponent::Model,
            ErrorCategory::Internal,
            "model.openai_compat.response_body_too_large",
            format!("response body exceeds the configured {} byte limit", limit.get()),
        )
        .with_provider(provider)
        .with_upstream_status(status)
    };
    // reqwest reports the decoded length when known, not an unchecked wire header.
    if response.content_length().is_some_and(|length| length > limit.get() as u64) {
        return Err(overflow());
    }
    let version = response.version();
    let headers = response.headers().clone();
    let mut body = Vec::new();
    loop {
        let chunk = match response.chunk().await {
            Ok(Some(chunk)) => chunk,
            Ok(None) => break,
            Err(_) if !response.status().is_success() => {
                // Preserve the status branch's empty error text on read failure.
                body.clear();
                break;
            }
            Err(_) => {
                return Err(AdkError::new(
                    ErrorComponent::Model,
                    ErrorCategory::Internal,
                    read_error_code,
                    "failed to read the response body",
                )
                .with_provider(provider)
                .with_upstream_status(status));
            }
        };
        if chunk.len() > limit.get() - body.len() {
            return Err(overflow());
        }
        body.extend_from_slice(&chunk);
    }
    // Retain reqwest's JSON/text codecs without reading the network body again.
    let mut bounded = http::Response::new(body);
    *bounded.status_mut() = response.status();
    *bounded.version_mut() = version;
    *bounded.headers_mut() = headers;
    Ok(bounded.into())
}
