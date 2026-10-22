//! Integration tests for the Monty executors: one-shot isolation, REPL
//! continuity, lifecycle, OS-call servicing against grants, host functions,
//! sandbox enforcement, and stdout capture.
//!
//! Gated behind `#[cfg(feature = "embedded-python")]` — run with:
//! ```bash
//! cargo nextest run -p adk-code --features embedded-python
//! ```
#![cfg(feature = "embedded-python")]

use std::sync::Arc;
use std::time::Duration;

use adk_code::{
    CodeExecutor, ExecutionError, ExecutionLanguage, ExecutionPayload, ExecutionRequest,
    ExecutionStatus, FilesystemPolicy, HostFunction, HostFunctionError, MontyExecutorBuilder,
    PathAccess, SandboxPolicy,
};
use async_trait::async_trait;
use serde_json::{Map, Value, json};

fn request(code: &str) -> ExecutionRequest {
    request_with(code, None, SandboxPolicy::strict_python())
}

fn request_with(code: &str, input: Option<Value>, sandbox: SandboxPolicy) -> ExecutionRequest {
    ExecutionRequest {
        language: ExecutionLanguage::Python,
        payload: ExecutionPayload::Source { code: code.to_string() },
        argv: vec![],
        stdin: None,
        input,
        sandbox,
        identity: None,
    }
}

// ---------------------------------------------------------------------------
// One-shot isolation
// ---------------------------------------------------------------------------

#[tokio::test]
async fn one_shot_calls_are_isolated() {
    let executor = MontyExecutorBuilder::new().build_one_shot().unwrap();

    let first = executor.execute(request("x = 1")).await.unwrap();
    assert_eq!(first.status, ExecutionStatus::Success);

    // The second call runs in a fresh interpreter: `x` is undefined.
    let second = executor.execute(request("x + 1")).await.unwrap();
    assert_eq!(second.status, ExecutionStatus::Failed);
    assert!(second.stderr.contains("NameError"), "stderr: {}", second.stderr);
}

#[tokio::test]
async fn one_shot_returns_final_expression_as_output() {
    let executor = MontyExecutorBuilder::new().build_one_shot().unwrap();
    let result = executor.execute(request("{'n': 21 * 2}")).await.unwrap();
    assert_eq!(result.status, ExecutionStatus::Success);
    assert_eq!(result.output, Some(json!({"n": 42})));
}

#[tokio::test]
async fn one_shot_binds_input_variable() {
    let executor = MontyExecutorBuilder::new().build_one_shot().unwrap();
    let result = executor
        .execute(request_with(
            "input['x'] + 1",
            Some(json!({"x": 41})),
            SandboxPolicy::strict_python(),
        ))
        .await
        .unwrap();
    assert_eq!(result.output, Some(json!(42)));
}

#[tokio::test]
async fn empty_source_is_rejected() {
    let executor = MontyExecutorBuilder::new().build_one_shot().unwrap();
    let err = executor.execute(request("   ")).await.unwrap_err();
    assert!(matches!(err, ExecutionError::InvalidRequest(_)));
}

#[tokio::test]
async fn one_shot_lifecycle_stays_no_op() {
    let executor = MontyExecutorBuilder::new().build_one_shot().unwrap();
    executor.start().await.unwrap();
    assert!(executor.is_running().await);
    executor.stop().await.unwrap();
    // No lifecycle state exists on the one-shot product.
    assert!(executor.is_running().await);
}

// ---------------------------------------------------------------------------
// REPL continuity and lifecycle
// ---------------------------------------------------------------------------

#[tokio::test]
async fn repl_state_persists_across_calls() {
    let executor = MontyExecutorBuilder::new().build_repl().unwrap();

    executor.execute(request("x = 1")).await.unwrap();
    let result = executor.execute(request("x + 1")).await.unwrap();
    assert_eq!(result.status, ExecutionStatus::Success);
    assert_eq!(result.output, Some(json!(2)));
}

#[tokio::test]
async fn repl_function_definitions_and_imports_persist() {
    let executor = MontyExecutorBuilder::new().environ_var("PROJECT", "acme").build_repl().unwrap();
    let sandbox = || {
        let mut policy = executor.granted_policy();
        policy.timeout = Duration::from_secs(30);
        policy
    };

    executor
        .execute(request_with("import os\ndef double(n):\n    return n * 2", None, sandbox()))
        .await
        .unwrap();
    let result = executor
        .execute(request_with("[double(21), os.getenv('PROJECT')]", None, sandbox()))
        .await
        .unwrap();
    assert_eq!(result.output, Some(json!([42, "acme"])));
}

#[tokio::test]
async fn repl_failed_snippet_preserves_session() {
    let executor = MontyExecutorBuilder::new().build_repl().unwrap();

    executor.execute(request("x = 5")).await.unwrap();
    let failed = executor.execute(request("1 / 0")).await.unwrap();
    assert_eq!(failed.status, ExecutionStatus::Failed);
    assert!(failed.stderr.contains("ZeroDivisionError"), "stderr: {}", failed.stderr);

    let result = executor.execute(request("x")).await.unwrap();
    assert_eq!(result.output, Some(json!(5)));
}

#[tokio::test]
async fn repl_lifecycle_manages_the_session() {
    let executor = MontyExecutorBuilder::new().build_repl().unwrap();
    assert!(!executor.is_running().await);

    executor.start().await.unwrap();
    assert!(executor.is_running().await);

    executor.execute(request("x = 1")).await.unwrap();
    executor.stop().await.unwrap();
    assert!(!executor.is_running().await);

    // The stopped session's state is gone; execute() lazily starts a new one.
    let result = executor.execute(request("x")).await.unwrap();
    assert_eq!(result.status, ExecutionStatus::Failed);
    assert!(result.stderr.contains("NameError"), "stderr: {}", result.stderr);

    executor.execute(request("y = 2")).await.unwrap();
    executor.restart().await.unwrap();
    assert!(executor.is_running().await);
    let result = executor.execute(request("y + 1")).await.unwrap();
    assert_eq!(result.status, ExecutionStatus::Failed);
    assert!(result.stderr.contains("NameError"), "stderr: {}", result.stderr);
}

#[tokio::test]
async fn repl_lazy_start_on_execute() {
    let executor = MontyExecutorBuilder::new().build_repl().unwrap();
    assert!(!executor.is_running().await);
    executor.execute(request("x = 1")).await.unwrap();
    assert!(executor.is_running().await);
}

#[tokio::test]
async fn repl_policy_must_not_vary_between_calls() {
    let executor = MontyExecutorBuilder::new().environ_var("PROJECT", "acme").build_repl().unwrap();

    // First call establishes the session policy (nothing requested).
    executor.execute(request("x = 1")).await.unwrap();

    // A different effective policy on a live session is rejected with
    // guidance, and the session stays usable under the original policy.
    let err =
        executor.execute(request_with("x", None, executor.granted_policy())).await.unwrap_err();
    match err {
        ExecutionError::InvalidRequest(msg) => assert!(msg.contains("restart")),
        other => panic!("expected InvalidRequest, got {other:?}"),
    }
    let result = executor.execute(request("x")).await.unwrap();
    assert_eq!(result.output, Some(json!(1)));

    // After a restart the new policy is accepted.
    executor.restart().await.unwrap();
    let result = executor
        .execute(request_with("import os\nos.getenv('PROJECT')", None, executor.granted_policy()))
        .await
        .unwrap();
    assert_eq!(result.output, Some(json!("acme")));
}

#[tokio::test]
async fn concurrent_repl_calls_serialize_and_both_apply() {
    let executor = Arc::new(MontyExecutorBuilder::new().build_repl().unwrap());

    let a = tokio::spawn({
        let executor = executor.clone();
        async move { executor.execute(request("x = 1")).await }
    });
    let b = tokio::spawn({
        let executor = executor.clone();
        async move { executor.execute(request("y = 2")).await }
    });
    a.await.unwrap().unwrap();
    b.await.unwrap().unwrap();

    let result = executor.execute(request("x + y")).await.unwrap();
    assert_eq!(result.output, Some(json!(3)));
}

// ---------------------------------------------------------------------------
// OS calls against grants
// ---------------------------------------------------------------------------

/// A policy requesting everything the executor grants, with a test timeout.
fn full_policy(granted: SandboxPolicy) -> SandboxPolicy {
    SandboxPolicy { timeout: Duration::from_secs(30), ..granted }
}

#[tokio::test]
async fn granted_read_only_mount_reads_but_rejects_writes() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("greeting.txt"), "hello").unwrap();

    let executor = MontyExecutorBuilder::new()
        .allow_path("/data", dir.path(), PathAccess::ReadOnly)
        .build_one_shot()
        .unwrap();
    let policy = full_policy(executor.granted_policy());

    let read = executor
        .execute(request_with(
            "from pathlib import Path\nPath('/data/greeting.txt').read_text()",
            None,
            policy.clone(),
        ))
        .await
        .unwrap();
    assert_eq!(read.output, Some(json!("hello")));

    let code = r#"
from pathlib import Path
result = 'unknown'
try:
    Path('/data/denied.txt').write_text('nope')
    result = 'wrote'
except OSError:
    result = 'denied'
result
"#;
    let write = executor.execute(request_with(code, None, policy)).await.unwrap();
    assert_eq!(write.output, Some(json!("denied")));
    assert!(!dir.path().join("denied.txt").exists());
}

#[tokio::test]
async fn granted_read_write_mount_writes_land_on_the_host_path() {
    let dir = tempfile::tempdir().unwrap();

    let executor = MontyExecutorBuilder::new()
        .allow_path("/out", dir.path(), PathAccess::ReadWrite)
        .build_one_shot()
        .unwrap();

    let result = executor
        .execute(request_with(
            "from pathlib import Path\nPath('/out/answer.txt').write_text('42')",
            None,
            full_policy(executor.granted_policy()),
        ))
        .await
        .unwrap();
    assert_eq!(result.status, ExecutionStatus::Success);
    assert_eq!(std::fs::read_to_string(dir.path().join("answer.txt")).unwrap(), "42");
}

#[tokio::test]
async fn out_of_grant_path_raises_os_error_in_script() {
    let dir = tempfile::tempdir().unwrap();
    let executor = MontyExecutorBuilder::new()
        .allow_path("/data", dir.path(), PathAccess::ReadOnly)
        .build_one_shot()
        .unwrap();

    let code = r#"
from pathlib import Path
result = 'unknown'
try:
    Path('/etc/passwd').read_text()
    result = 'read'
except OSError:
    result = 'denied'
result
"#;
    let result = executor
        .execute(request_with(code, None, full_policy(executor.granted_policy())))
        .await
        .unwrap();
    assert_eq!(result.output, Some(json!("denied")));
}

#[tokio::test]
async fn request_exceeding_grants_is_rejected_before_code_runs() {
    let executor = MontyExecutorBuilder::new().build_one_shot().unwrap();
    let mut policy = SandboxPolicy::strict_python();
    policy.filesystem = FilesystemPolicy::WorkspaceReadOnly { root: "/data".into() };

    let err = executor.execute(request_with("1 + 1", None, policy)).await.unwrap_err();
    match err {
        ExecutionError::UnsupportedPolicy(msg) => assert!(msg.contains("/data")),
        other => panic!("expected UnsupportedPolicy, got {other:?}"),
    }
}

#[tokio::test]
async fn getenv_returns_granted_vars_and_none_otherwise() {
    let executor =
        MontyExecutorBuilder::new().environ_var("PROJECT", "acme").build_one_shot().unwrap();

    let result = executor
        .execute(request_with(
            "import os\n[os.getenv('PROJECT'), os.getenv('MISSING')]",
            None,
            full_policy(executor.granted_policy()),
        ))
        .await
        .unwrap();
    assert_eq!(result.output, Some(json!(["acme", null])));
}

#[tokio::test]
async fn clock_grant_controls_datetime_now() {
    let clock_code = r#"
from datetime import datetime
result = 'unknown'
try:
    result = datetime.now().year
except OSError:
    result = 'denied'
result
"#;

    let granted = MontyExecutorBuilder::new().system_clock().build_one_shot().unwrap();
    let result = granted.execute(request(clock_code)).await.unwrap();
    let year = result.output.as_ref().and_then(Value::as_i64).expect("a year");
    assert!(year >= 2025, "year: {year}");

    let denied = MontyExecutorBuilder::new().build_one_shot().unwrap();
    let result = denied.execute(request(clock_code)).await.unwrap();
    assert_eq!(result.output, Some(json!("denied")));
}

// ---------------------------------------------------------------------------
// Host functions — one-shot
// ---------------------------------------------------------------------------

struct Combine;

#[async_trait]
impl HostFunction for Combine {
    fn name(&self) -> &str {
        "combine"
    }
    fn description(&self) -> &str {
        "Echo positional and keyword arguments."
    }
    async fn call(
        &self,
        args: Vec<Value>,
        kwargs: Map<String, Value>,
    ) -> Result<Value, HostFunctionError> {
        Ok(json!({ "args": args, "kwargs": kwargs }))
    }
}

#[tokio::test]
async fn host_function_receives_args_and_returns_value() {
    let executor =
        MontyExecutorBuilder::new().function(Arc::new(Combine)).build_one_shot().unwrap();

    let result = executor.execute(request("combine(1, 'two', flag=True, n=3)")).await.unwrap();
    assert_eq!(result.status, ExecutionStatus::Success);
    assert_eq!(
        result.output,
        Some(json!({ "args": [1, "two"], "kwargs": { "flag": true, "n": 3 } }))
    );
}

#[tokio::test]
async fn async_host_function_is_awaited_by_the_host() {
    let executor = MontyExecutorBuilder::new()
        .function_fn("slow_double", "Double a number, slowly.", |args, _kwargs| async move {
            tokio::time::sleep(Duration::from_millis(20)).await;
            let n = args.first().and_then(Value::as_i64).unwrap_or(0);
            Ok(json!(n * 2))
        })
        .build_one_shot()
        .unwrap();

    let result = executor.execute(request("slow_double(21)")).await.unwrap();
    assert_eq!(result.output, Some(json!(42)));
}

#[tokio::test]
async fn host_function_error_is_catchable_in_script() {
    let executor = MontyExecutorBuilder::new()
        .function_fn("fail", "Always fails.", |_args, _kwargs| async move {
            Err(HostFunctionError::new("boom: try something else"))
        })
        .build_one_shot()
        .unwrap();

    let code = r#"
result = 'unknown'
try:
    fail()
    result = 'ok'
except RuntimeError as e:
    result = str(e)
result
"#;
    let result = executor.execute(request(code)).await.unwrap();
    assert_eq!(result.status, ExecutionStatus::Success);
    assert_eq!(result.output, Some(json!("boom: try something else")));
}

#[tokio::test]
async fn unknown_function_raises_corrective_exception_listing_registered_names() {
    let executor =
        MontyExecutorBuilder::new()
            .function_fn("known", "A registered function.", |_args, _kwargs| async move {
                Ok(json!(null))
            })
            .build_one_shot()
            .unwrap();

    let result = executor.execute(request("mystery(1)")).await.unwrap();
    assert_eq!(result.status, ExecutionStatus::Failed);
    assert!(result.stderr.contains("mystery"), "stderr: {}", result.stderr);
    assert!(result.stderr.contains("known"), "stderr: {}", result.stderr);
}

#[tokio::test]
async fn hung_host_function_times_out_as_in_script_exception() {
    let executor = MontyExecutorBuilder::new()
        .function_fn("hang", "Never returns.", |_args, _kwargs| async move {
            tokio::time::sleep(Duration::from_secs(3600)).await;
            Ok(json!(null))
        })
        .host_function_timeout(Duration::from_millis(50))
        .build_one_shot()
        .unwrap();

    let code = r#"
result = 'unknown'
try:
    hang()
    result = 'ok'
except RuntimeError as e:
    result = str(e)
result
"#;
    let result = executor.execute(request(code)).await.unwrap();
    assert_eq!(result.status, ExecutionStatus::Success);
    let message = result.output.as_ref().and_then(Value::as_str).expect("a message");
    assert!(message.contains("timed out"), "message: {message}");
}

// ---------------------------------------------------------------------------
// Host functions — REPL
// ---------------------------------------------------------------------------

#[tokio::test]
async fn repl_host_function_works_across_calls_and_results_persist() {
    let executor = MontyExecutorBuilder::new()
        .function_fn("double", "Double a number.", |args, _kwargs| async move {
            let n = args.first().and_then(Value::as_i64).unwrap_or(0);
            Ok(json!(n * 2))
        })
        .build_repl()
        .unwrap();

    // First call: `double` resolves via NameLookup, then pauses at the call.
    let first = executor.execute(request("n = double(10)\nn")).await.unwrap();
    assert_eq!(first.output, Some(json!(20)));

    // Second call: the cached Function object is invoked directly, and the
    // previous call's result is still in the namespace.
    let second = executor.execute(request("double(n) + n")).await.unwrap();
    assert_eq!(second.output, Some(json!(60)));
}

// ---------------------------------------------------------------------------
// Sandbox enforcement
// ---------------------------------------------------------------------------

#[tokio::test]
async fn interpreter_timeout_produces_timeout_status() {
    let executor = MontyExecutorBuilder::new().build_one_shot().unwrap();
    let mut policy = SandboxPolicy::strict_python();
    policy.timeout = Duration::from_millis(200);

    let result =
        executor.execute(request_with("while True:\n    pass", None, policy)).await.unwrap();
    assert_eq!(result.status, ExecutionStatus::Timeout);
    assert!(!result.stderr.is_empty());
}

#[tokio::test]
async fn memory_cap_produces_failed_status() {
    let executor = MontyExecutorBuilder::new().max_memory(1024 * 1024).build_one_shot().unwrap();

    let result = executor.execute(request("x = [0] * 10_000_000\nlen(x)")).await.unwrap();
    assert_eq!(result.status, ExecutionStatus::Failed);
    assert!(result.stderr.contains("MemoryError"), "stderr: {}", result.stderr);
}

// ---------------------------------------------------------------------------
// stdout capture and truncation
// ---------------------------------------------------------------------------

#[tokio::test]
async fn stdout_concatenates_across_segments_in_order() {
    let executor = MontyExecutorBuilder::new()
        .function_fn("echo", "Echo the first argument.", |args, _kwargs| async move {
            Ok(args.into_iter().next().unwrap_or(Value::Null))
        })
        .build_one_shot()
        .unwrap();

    // The host-function call splits the drive into two blocking segments;
    // print output from both must arrive in order.
    let result =
        executor.execute(request("print('one')\nr = echo(1)\nprint('two')\nr")).await.unwrap();
    assert_eq!(result.stdout, "one\ntwo\n");
    assert!(!result.stdout_truncated);
}

#[tokio::test]
async fn stdout_is_truncated_to_the_policy_limit() {
    let executor = MontyExecutorBuilder::new().build_one_shot().unwrap();
    let mut policy = SandboxPolicy::strict_python();
    policy.max_stdout_bytes = 8;

    let result = executor.execute(request_with("print('a' * 100)", None, policy)).await.unwrap();
    assert!(result.stdout_truncated);
    assert_eq!(result.stdout.len(), 8);
}

#[tokio::test]
async fn print_loop_is_capped_during_the_drive_and_keeps_running() {
    // The cap is enforced *while* the script runs — a print loop cannot grow
    // host memory past the policy limit, and the run still completes.
    let executor = MontyExecutorBuilder::new().build_one_shot().unwrap();
    let mut policy = SandboxPolicy::strict_python();
    policy.max_stdout_bytes = 1024;

    let code = "for i in range(10_000):\n    print('x' * 100)\n'done'";
    let result = executor.execute(request_with(code, None, policy)).await.unwrap();
    assert_eq!(result.status, ExecutionStatus::Success);
    assert_eq!(result.output, Some(json!("done")));
    assert!(result.stdout_truncated);
    assert_eq!(result.stdout.len(), 1024);
}

// ---------------------------------------------------------------------------
// Conversion depth and grant coverage
// ---------------------------------------------------------------------------

#[tokio::test]
async fn iteratively_built_deep_nesting_degrades_instead_of_crashing() {
    // A script can nest 2000 levels with trivial memory and no
    // RecursionError; converting the final value must not overflow the
    // host stack (which would abort the whole process).
    let executor = MontyExecutorBuilder::new().build_one_shot().unwrap();

    let code = "x = [1]\nfor _ in range(150):\n    x = [x]\nx";
    let result = executor.execute(request(code)).await.unwrap();
    assert_eq!(result.status, ExecutionStatus::Success);
    let output = result.output.expect("a converted value");
    assert!(output.is_array());
    assert!(output.to_string().contains("nesting depth limit reached"));
}

#[tokio::test]
async fn subdirectory_of_a_grant_is_readable_and_scopes_out_the_rest() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir(dir.path().join("sub")).unwrap();
    std::fs::write(dir.path().join("sub/inner.txt"), "inner").unwrap();
    std::fs::write(dir.path().join("outer.txt"), "outer").unwrap();

    let executor = MontyExecutorBuilder::new()
        .allow_path("/data", dir.path(), PathAccess::ReadOnly)
        .build_one_shot()
        .unwrap();
    // Narrow the grant to its /data/sub subtree for this call.
    let mut policy = full_policy(executor.granted_policy());
    policy.filesystem = FilesystemPolicy::WorkspaceReadOnly { root: "/data/sub".into() };

    let read = executor
        .execute(request_with(
            "from pathlib import Path\nPath('/data/sub/inner.txt').read_text()",
            None,
            policy.clone(),
        ))
        .await
        .unwrap();
    assert_eq!(read.output, Some(json!("inner")));

    // The rest of the granted tree is out of scope for this call.
    let code = r#"
from pathlib import Path
result = 'unknown'
try:
    Path('/data/outer.txt').read_text()
    result = 'read'
except OSError:
    result = 'denied'
result
"#;
    let outer = executor.execute(request_with(code, None, policy)).await.unwrap();
    assert_eq!(outer.output, Some(json!("denied")));
}

#[tokio::test]
async fn working_directory_request_is_rejected_before_code_runs() {
    let executor = MontyExecutorBuilder::new().build_one_shot().unwrap();
    let mut policy = SandboxPolicy::strict_python();
    policy.working_directory = Some("/work".into());

    let err = executor.execute(request_with("1 + 1", None, policy)).await.unwrap_err();
    match err {
        ExecutionError::UnsupportedPolicy(msg) => assert!(msg.contains("working_directory")),
        other => panic!("expected UnsupportedPolicy, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// Await denial and method-call misuse
// ---------------------------------------------------------------------------

#[tokio::test]
async fn awaiting_a_host_function_result_is_a_catchable_script_error() {
    // Host functions resolve synchronously from the script's perspective:
    // the call pauses the drive, the host awaits the function, and the
    // interpreter resumes with the concrete value — so `await`ing it is an
    // ordinary TypeError (data), never a host error.
    let executor = MontyExecutorBuilder::new()
        .function_fn("fetch", "Fetch a value.", |_args, _kwargs| async move { Ok(json!(7)) })
        .build_one_shot()
        .unwrap();

    let result = executor.execute(request("x = await fetch()\nx")).await.unwrap();
    assert_eq!(result.status, ExecutionStatus::Failed);
    assert!(result.stderr.contains("TypeError"), "stderr: {}", result.stderr);
}

#[tokio::test]
async fn registered_function_called_as_method_raises_a_catchable_error() {
    // Monty resolves attribute access on built-in types itself, so a
    // method-style invocation of a registered host function surfaces as an
    // ordinary catchable AttributeError — host functions are bare names only.
    let executor = MontyExecutorBuilder::new()
        .function_fn("double", "Double a number.", |args, _kwargs| async move {
            let n = args.first().and_then(Value::as_i64).unwrap_or(0);
            Ok(json!(n * 2))
        })
        .build_one_shot()
        .unwrap();

    let code = r#"
result = 'unknown'
try:
    result = 'value: ' + str({}.double(2))
except Exception as e:
    result = 'error: ' + str(e)
result
"#;
    let result = executor.execute(request(code)).await.unwrap();
    assert_eq!(result.status, ExecutionStatus::Success);
    let message = result.output.as_ref().and_then(Value::as_str).expect("a message");
    assert!(message.contains("error:") && message.contains("double"), "message: {message}");
}
