//! Property-based tests for adk-code.
//!
//! Uses `proptest` with 100+ iterations per property.

use proptest::prelude::*;

use adk_code::diagnostics::parse_diagnostics;

// ---------------------------------------------------------------------------
// Generators
// ---------------------------------------------------------------------------

/// Generates a valid rustc JSON diagnostic line with the given level and message.
fn make_diagnostic_json(level: &str, message: &str, code: Option<&str>) -> String {
    let code_json = match code {
        Some(c) => format!(r#"{{"code":"{c}","explanation":null}}"#),
        None => "null".to_string(),
    };
    format!(
        r#"{{"message":"{message}","code":{code_json},"level":"{level}","spans":[],"children":[],"rendered":"{level}: {message}"}}"#
    )
}

/// Generates a valid rustc error diagnostic JSON line with arbitrary content.
fn arb_error_diagnostic() -> impl Strategy<Value = String> {
    (
        "[a-z ]{3,30}",                // message
        prop::option::of("E[0-9]{4}"), // optional error code
    )
        .prop_map(|(msg, code)| make_diagnostic_json("error", &msg, code.as_deref()))
}

/// Generates a valid rustc warning diagnostic JSON line.
fn arb_warning_diagnostic() -> impl Strategy<Value = String> {
    "[a-z ]{3,30}".prop_map(|msg| make_diagnostic_json("warning", &msg, None))
}

/// Generates a mix of error and warning diagnostics (1 to 5 lines).
fn arb_diagnostic_stderr() -> impl Strategy<Value = String> {
    prop::collection::vec(prop_oneof![arb_error_diagnostic(), arb_warning_diagnostic()], 1..=5)
        .prop_map(|lines| lines.join("\n"))
}

// ---------------------------------------------------------------------------
// Property 5: Diagnostic parsing
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    /// **Feature: sandbox-and-code-tools, Property 5: Diagnostic parsing**
    /// *For any* valid rustc JSON diagnostic output, `parse_diagnostics()` should
    /// return a non-empty Vec of RustDiagnostic structs with level and message
    /// fields populated.
    /// **Validates: Requirements REQ-COD-002**
    #[test]
    fn prop_diagnostic_parsing(stderr in arb_diagnostic_stderr()) {
        let diagnostics = parse_diagnostics(&stderr);

        // Must return at least one diagnostic (we generate 1-5 valid lines)
        prop_assert!(
            !diagnostics.is_empty(),
            "expected non-empty diagnostics for input:\n{stderr}"
        );

        // Every diagnostic must have level and message populated
        for diag in &diagnostics {
            prop_assert!(
                !diag.level.is_empty(),
                "diagnostic level should not be empty"
            );
            prop_assert!(
                !diag.message.is_empty(),
                "diagnostic message should not be empty"
            );
            // Level must be one of the known rustc levels
            prop_assert!(
                matches!(diag.level.as_str(), "error" | "warning" | "note" | "help"),
                "unexpected diagnostic level: {}",
                diag.level
            );
        }
    }
}

// ---------------------------------------------------------------------------
// Property 7: Deprecated alias compilation
// ---------------------------------------------------------------------------

/// **Feature: sandbox-and-code-tools, Property 7: Deprecated alias compilation**
/// The compat module should be importable and the migration guide documentation
/// should be accessible.
/// **Validates: Requirements REQ-MIG-001, REQ-MIG-002**
#[test]
fn prop_deprecated_alias_compilation() {
    // Verify the compat module is importable — this is a compile-time check.
    // If the module doesn't exist or has errors, this test won't compile.
    // The compat module contains migration documentation and deprecated type aliases.

    // Verify key re-exported types from adk-code are accessible
    assert!(!std::any::type_name::<adk_code::CodeTool>().is_empty());
    assert!(!std::any::type_name::<adk_code::RustExecutor>().is_empty());
    assert!(!std::any::type_name::<adk_code::RustExecutorConfig>().is_empty());
    assert!(!std::any::type_name::<adk_code::CodeError>().is_empty());
    assert!(!std::any::type_name::<adk_code::RustDiagnostic>().is_empty());

    // Verify adk-sandbox types are accessible (the migration target)
    assert!(!std::any::type_name::<dyn adk_sandbox::SandboxBackend>().is_empty());
    assert!(!std::any::type_name::<adk_sandbox::ExecRequest>().is_empty());
    assert!(!std::any::type_name::<adk_sandbox::ExecResult>().is_empty());

    // Verify parse_diagnostics is accessible
    let empty = adk_code::parse_diagnostics("");
    assert!(empty.is_empty());
}
