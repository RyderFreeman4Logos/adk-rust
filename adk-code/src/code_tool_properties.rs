//! Error-as-information properties against the actual private tool renderer.

use proptest::prelude::*;

use super::code_error_to_json;
use crate::CodeError;

/// Generates arbitrary CodeError variants.
fn arb_code_error() -> impl Strategy<Value = CodeError> {
    prop_oneof![
        // CompileError
        ("[a-z ]{3,30}", prop::option::of("E[0-9]{4}"),).prop_map(|(msg, code)| {
            let diag = crate::RustDiagnostic {
                level: "error".to_string(),
                message: msg.clone(),
                spans: vec![],
                code,
            };
            CodeError::CompileError { diagnostics: vec![diag], stderr: format!("error: {msg}") }
        }),
        // DependencyNotFound
        "[a-z_]{3,15}".prop_map(|name| CodeError::DependencyNotFound {
            name,
            searched: vec!["config: /fake".to_string()],
        }),
        // Sandbox errors
        (1u64..=300).prop_map(|secs| {
            CodeError::Sandbox(adk_sandbox::SandboxError::Timeout {
                timeout: std::time::Duration::from_secs(secs),
            })
        }),
        (1u32..=1024).prop_map(|mb| {
            CodeError::Sandbox(adk_sandbox::SandboxError::MemoryExceeded { limit_mb: mb })
        }),
        "[a-z ]{3,30}".prop_map(|msg| {
            CodeError::Sandbox(adk_sandbox::SandboxError::ExecutionFailed(msg))
        }),
        "[a-z ]{3,30}"
            .prop_map(|msg| { CodeError::Sandbox(adk_sandbox::SandboxError::InvalidRequest(msg)) }),
        "[a-z ]{3,30}".prop_map(|msg| {
            CodeError::Sandbox(adk_sandbox::SandboxError::BackendUnavailable(msg))
        }),
        // InvalidCode
        "[a-z ]{3,30}".prop_map(CodeError::InvalidCode),
    ]
}

// ---------------------------------------------------------------------------
// Property 6: Error-as-information
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    /// **Feature: sandbox-and-code-tools, Property 6: Error-as-information**
    /// *For any* CodeError variant, converting it to a JSON tool output should
    /// always produce a valid JSON object with "status" and "error"/"stderr"
    /// fields, never a ToolError.
    /// **Validates: Requirements REQ-ERR-002**
    #[test]
    fn prop_error_as_information(err in arb_code_error()) {
        let json = code_error_to_json(&err);

        // Must be a JSON object
        prop_assert!(json.is_object(), "expected JSON object, got: {json}");

        // Must have a "status" field
        prop_assert!(
            json.get("status").is_some(),
            "missing 'status' field in: {json}"
        );

        let status = json["status"].as_str().unwrap();
        prop_assert!(
            !status.is_empty(),
            "status field should not be empty"
        );

        // Must have a "stderr" or "diagnostics" field for error context
        let has_stderr = json.get("stderr").is_some();
        let has_diagnostics = json.get("diagnostics").is_some();
        prop_assert!(
            has_stderr || has_diagnostics,
            "expected 'stderr' or 'diagnostics' field in: {json}"
        );

        // Status must be one of the known values
        prop_assert!(
            matches!(status, "compile_error" | "error" | "timeout" | "memory_exceeded"),
            "unexpected status: {status}"
        );
    }
}
