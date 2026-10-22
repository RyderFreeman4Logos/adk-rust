//! `MontyPythonCodeTool` — in-process Python execution via the Monty interpreter.
//!
//! This tool wraps the `adk-code` Monty executors (`MontyOneShotExecutor` /
//! `MontyReplExecutor`) to run Python snippets in-process — no container, no
//! subprocess, microsecond startup. It supports two modes:
//!
//! - **One-shot** (default): each call runs in a fresh interpreter.
//! - **REPL**: interpreter state (variables, functions, imports) persists
//!   across calls, scoped per ADK session.
//!
//! It complements [`PythonCodeTool`](super::PythonCodeTool), which runs full
//! CPython in an isolated container — use that when scripts need the real
//! Python ecosystem (pip packages, C extensions, the complete standard
//! library). Monty implements a subset of Python, in exchange for in-process
//! speed, serializable interpreter state, and a no-network/no-subprocess
//! guarantee that holds by construction.
//!
//! When the `code-embedded-python` feature is not enabled, the tool returns a
//! structured `"rejected"` result explaining how to enable it.
//!
//! # Required Scopes
//!
//! This tool declares `["code:execute"]` as its required scope. Embedded
//! Python runs in-process with only the OS access granted at construction, so
//! no elevated container or host scopes are needed.

use adk_core::{Result, Tool, ToolContext};
use async_trait::async_trait;
use serde_json::{Value, json};
use std::sync::Arc;

/// Default `timeout_secs` when the model omits it.
const DEFAULT_TIMEOUT_SECS: u64 = 30;
/// Bounds for the model-supplied `timeout_secs` (values outside are clamped).
const MIN_TIMEOUT_SECS: u64 = 1;
const MAX_TIMEOUT_SECS: u64 = 300;

/// The short base contract; the executor's own `prompt_snippet()` is appended
/// at construction, so the description always reflects the built environment.
const BASE_DESCRIPTION: &str = "Execute Python code in a sandboxed interpreter.";

/// Embedded Python code execution tool backed by the Monty interpreter.
///
/// The LLM-facing description is **composed, not authored**: a short base
/// contract followed by the executor's own
/// [`prompt_snippet`](adk_code::CodeExecutor::prompt_snippet), which describes
/// mode semantics, granted filesystem roots, environment variable names
/// (never values), clock availability, the no-network/no-subprocess
/// guarantee, and registered host functions. Because both the description and
/// the in-interpreter behavior derive from the same configuration, they
/// cannot drift.
///
/// In REPL mode, interpreter sessions are keyed by the full ADK session
/// identity — app name, user id, and session id — so state never leaks
/// between users even when session id strings repeat across users. The
/// session map is bounded by an LRU cap (default 100); an evicted session's
/// next call transparently starts a fresh interpreter.
///
/// For container-backed CPython (pip packages, C extensions, the complete
/// standard library), use [`PythonCodeTool`](super::PythonCodeTool) instead.
///
/// Requires the `code-embedded-python` feature. Without it, the tool returns
/// a structured `"rejected"` result.
///
/// # Required Scopes
///
/// Returns `["code:execute"]`. No elevated scopes needed.
///
/// # Example
///
/// ```rust
/// use adk_tool::{MontyPythonCodeTool, Tool};
///
/// let tool = MontyPythonCodeTool::new();
/// assert_eq!(tool.name(), "monty_python_code");
/// assert_eq!(tool.required_scopes(), &["code:execute"]);
/// ```
pub struct MontyPythonCodeTool {
    repl: bool,
    description: String,
    #[cfg(feature = "code-embedded-python")]
    inner: enabled::Inner,
}

impl MontyPythonCodeTool {
    /// Create a one-shot tool with a fully sandboxed interpreter: no
    /// filesystem, no environment variables, no clock, no host functions.
    pub fn new() -> Self {
        Self::sandboxed(false)
    }

    /// Create a REPL-mode tool with a fully sandboxed interpreter. Variables,
    /// functions, and imports persist across calls within an ADK session.
    pub fn repl() -> Self {
        Self::sandboxed(true)
    }

    #[cfg(feature = "code-embedded-python")]
    fn sandboxed(repl: bool) -> Self {
        // A fully sandboxed builder has no host functions, so registry
        // validation cannot fail.
        let builder = MontyPythonCodeToolBuilder::new();
        let built = if repl { builder.build_repl() } else { builder.build_one_shot() };
        built.expect("an empty host-function registry always validates")
    }

    #[cfg(not(feature = "code-embedded-python"))]
    fn sandboxed(repl: bool) -> Self {
        Self { repl, description: BASE_DESCRIPTION.to_string() }
    }

    /// Start configuring a tool with OS-access grants, host functions, and
    /// limits (forwarded to
    /// [`MontyExecutorBuilder`](adk_code::MontyExecutorBuilder)).
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// use adk_code::PathAccess;
    /// use adk_tool::MontyPythonCodeTool;
    ///
    /// let tool = MontyPythonCodeTool::builder()
    ///     .allow_path("/data", "/srv/agent/data", PathAccess::ReadOnly)
    ///     .environ_var("PROJECT", "acme")
    ///     .system_clock()
    ///     .build_repl()?;
    /// ```
    #[cfg(feature = "code-embedded-python")]
    pub fn builder() -> MontyPythonCodeToolBuilder {
        MontyPythonCodeToolBuilder::new()
    }
}

impl Default for MontyPythonCodeTool {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Tool for MontyPythonCodeTool {
    fn name(&self) -> &str {
        "monty_python_code"
    }

    fn description(&self) -> &str {
        &self.description
    }

    fn required_scopes(&self) -> &[&str] {
        &["code:execute"]
    }

    /// The schema is built from configuration: `reset` exists only in REPL
    /// mode, where it discards the persistent session before executing.
    fn parameters_schema(&self) -> Option<Value> {
        let mut properties = json!({
            "code": {
                "type": "string",
                "description": "Python source to execute."
            },
            "input": {
                "description": "Optional JSON value bound to the `input` variable."
            },
            "timeout_secs": {
                "type": "integer",
                "default": DEFAULT_TIMEOUT_SECS,
                "minimum": MIN_TIMEOUT_SECS,
                "maximum": MAX_TIMEOUT_SECS,
                "description": "Time budget in seconds (default 30; values outside 1-300 \
                                are clamped)."
            }
        });
        if self.repl {
            properties["reset"] = json!({
                "type": "boolean",
                "default": false,
                "description": "REPL mode only: discard the persistent session before executing."
            });
        }
        Some(json!({
            "type": "object",
            "properties": properties,
            "required": ["code"]
        }))
    }

    /// The response envelope is fixed across both modes (and the feature-off
    /// fallback), so the schema is declared for the model. Script-level
    /// failures are data in this envelope, never a `ToolError`.
    fn response_schema(&self) -> Option<Value> {
        Some(json!({
            "type": "object",
            "properties": {
                "status": {
                    "type": "string",
                    "enum": ["success", "failed", "timeout", "rejected"],
                    "description": "'success': the script completed. 'failed': a Python \
                                    exception propagated (traceback in stderr). 'timeout': \
                                    the time budget was exceeded. 'rejected': bad arguments \
                                    or the feature is disabled."
                },
                "stdout": {
                    "type": "string",
                    "description": "Captured print() output."
                },
                "stderr": {
                    "type": "string",
                    "description": "Python traceback or rejection reason; empty on success."
                },
                "output": {
                    "description": "JSON value of the script's final expression; null unless \
                                    status is 'success'."
                },
                "stdoutTruncated": {
                    "type": "boolean",
                    "description": "True when stdout was cut at the policy limit."
                },
                "stderrTruncated": {
                    "type": "boolean",
                    "description": "True when stderr was cut at the policy limit."
                },
                "durationMs": {
                    "type": "integer",
                    "description": "Wall-clock execution time in milliseconds."
                }
            },
            "required": ["status", "stdout", "stderr", "output", "stdoutTruncated",
                         "stderrTruncated", "durationMs"]
        }))
    }

    fn is_read_only(&self) -> bool {
        // Scripts can write to granted mounts; host functions may have side
        // effects.
        false
    }

    fn is_concurrency_safe(&self) -> bool {
        // REPL snippet ordering within a session is meaningful; the
        // per-executor Mutex guarantees serialization, but parallel dispatch
        // would make ordering nondeterministic.
        !self.repl
    }

    async fn execute(&self, ctx: Arc<dyn ToolContext>, args: Value) -> Result<Value> {
        let Some(code) = args.get("code").and_then(Value::as_str) else {
            return Ok(rejected("missing required field: code"));
        };
        let input = args.get("input").cloned();
        let timeout_secs = args
            .get("timeout_secs")
            .and_then(Value::as_u64)
            .unwrap_or(DEFAULT_TIMEOUT_SECS)
            .clamp(MIN_TIMEOUT_SECS, MAX_TIMEOUT_SECS);
        let reset = args.get("reset").and_then(Value::as_bool).unwrap_or(false);

        self.execute_python(ctx, code, input, timeout_secs, reset).await
    }
}

/// A structured rejection envelope (bad arguments or feature disabled).
/// Never a `ToolError` — errors are information the model can react to.
fn rejected(message: &str) -> Value {
    json!({
        "status": "rejected",
        "stdout": "",
        "stderr": message,
        "output": null,
        "stdoutTruncated": false,
        "stderrTruncated": false,
        "durationMs": 0,
    })
}

#[cfg(feature = "code-embedded-python")]
pub use enabled::MontyPythonCodeToolBuilder;

#[cfg(feature = "code-embedded-python")]
mod enabled {
    use super::*;
    use adk_code::{
        CodeExecutor, ExecutionError, ExecutionLanguage, ExecutionPayload, ExecutionRequest,
        ExecutionResult, HostFunction, HostFunctionError, MontyBuildError, MontyExecutorBuilder,
        MontyOneShotExecutor, MontyReplExecutor, PathAccess,
    };
    use serde_json::Map;
    use std::collections::HashMap;
    use std::future::Future;
    use std::path::PathBuf;
    use std::time::Duration;
    use tokio::sync::RwLock;
    use tracing::debug;

    /// Default cap on concurrently retained REPL sessions.
    const DEFAULT_MAX_SESSIONS: usize = 100;

    /// The full ADK session identity. Session id strings are only unique per
    /// user, so REPL interpreter state is keyed by the whole triple — two
    /// users with the same session id never share an interpreter.
    #[derive(Clone, PartialEq, Eq, Hash)]
    pub(super) struct SessionKey {
        app: String,
        user: String,
        session: String,
    }

    impl SessionKey {
        pub(super) fn new(ctx: &dyn ToolContext) -> Self {
            Self {
                app: ctx.app_name().to_string(),
                user: ctx.user_id().to_string(),
                session: ctx.session_id().to_string(),
            }
        }
    }

    /// The live (feature-enabled) tool internals.
    pub(super) struct Inner {
        /// Grants, registry, limits — cheaply cloneable; mints per-session
        /// REPL executors.
        builder: MontyExecutorBuilder,
        /// A policy requesting exactly the tool's grants; the model controls
        /// only the code, the input, and the time budget.
        granted: adk_code::SandboxPolicy,
        /// One-shot mode: a single shared stateless executor.
        oneshot: Option<Arc<MontyOneShotExecutor>>,
        /// REPL mode: one interpreter session per ADK session identity,
        /// LRU-bounded.
        sessions: RwLock<LruSessions>,
    }

    /// A small LRU map: session identity → REPL executor, evicting the least
    /// recently used entry once `capacity` is reached. O(n) eviction is fine
    /// at the default capacity of 100.
    pub(super) struct LruSessions {
        capacity: usize,
        clock: u64,
        entries: HashMap<SessionKey, (Arc<MontyReplExecutor>, u64)>,
    }

    impl LruSessions {
        fn new(capacity: usize) -> Self {
            Self { capacity: capacity.max(1), clock: 0, entries: HashMap::new() }
        }

        fn get(&mut self, key: &SessionKey) -> Option<Arc<MontyReplExecutor>> {
            self.clock += 1;
            let clock = self.clock;
            self.entries.get_mut(key).map(|(executor, used)| {
                *used = clock;
                executor.clone()
            })
        }

        fn insert(&mut self, key: SessionKey, executor: Arc<MontyReplExecutor>) {
            self.clock += 1;
            if !self.entries.contains_key(&key) && self.entries.len() >= self.capacity {
                // Evict the least recently used entry; its session's next
                // call transparently starts a fresh interpreter.
                if let Some(evict) = self
                    .entries
                    .iter()
                    .min_by_key(|(_, (_, used))| *used)
                    .map(|(key, _)| key.clone())
                {
                    debug!(
                        session.app = %evict.app,
                        session.user = %evict.user,
                        session.id = %evict.session,
                        "evicting lru repl session"
                    );
                    self.entries.remove(&evict);
                }
            }
            self.entries.insert(key, (executor, self.clock));
        }

        fn remove(&mut self, key: &SessionKey) {
            self.entries.remove(key);
        }

        #[cfg(test)]
        fn contains(&self, key: &SessionKey) -> bool {
            self.entries.contains_key(key)
        }
    }

    /// Configures a [`MontyPythonCodeTool`]: OS-access grants, host
    /// functions, and limits are forwarded to [`MontyExecutorBuilder`]; the
    /// terminal methods select the mode.
    ///
    /// # Example
    ///
    /// ```rust
    /// use adk_code::PathAccess;
    /// use adk_tool::{MontyPythonCodeTool, Tool};
    ///
    /// # fn main() -> Result<(), adk_code::MontyBuildError> {
    /// let tool = MontyPythonCodeTool::builder()
    ///     .environ_var("PROJECT", "acme")
    ///     .system_clock()
    ///     .build_repl()?;
    /// assert!(tool.description().contains("PROJECT"));
    /// # Ok(())
    /// # }
    /// ```
    pub struct MontyPythonCodeToolBuilder {
        builder: MontyExecutorBuilder,
        max_sessions: usize,
    }

    impl Default for MontyPythonCodeToolBuilder {
        fn default() -> Self {
            Self::new()
        }
    }

    impl MontyPythonCodeToolBuilder {
        /// Start with the fully sandboxed defaults.
        #[must_use]
        pub fn new() -> Self {
            Self {
                builder: MontyExecutorBuilder::new().script_name("agent_snippet"),
                max_sessions: DEFAULT_MAX_SESSIONS,
            }
        }

        /// Make a host directory available to scripts at `virtual_path`.
        /// See [`MontyExecutorBuilder::allow_path`].
        #[must_use]
        pub fn allow_path(
            mut self,
            virtual_path: impl Into<String>,
            host_path: impl Into<PathBuf>,
            access: PathAccess,
        ) -> Self {
            self.builder = self.builder.allow_path(virtual_path, host_path, access);
            self
        }

        /// Replace the environment map exposed via `os.getenv` / `os.environ`.
        /// See [`MontyExecutorBuilder::environ`].
        #[must_use]
        pub fn environ<K, V>(mut self, vars: impl IntoIterator<Item = (K, V)>) -> Self
        where
            K: Into<String>,
            V: Into<String>,
        {
            self.builder = self.builder.environ(vars);
            self
        }

        /// Add or overwrite a single environment variable.
        /// See [`MontyExecutorBuilder::environ_var`].
        #[must_use]
        pub fn environ_var(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
            self.builder = self.builder.environ_var(key, value);
            self
        }

        /// Grant host-clock access (`date.today()` / `datetime.now()`).
        /// See [`MontyExecutorBuilder::system_clock`].
        #[must_use]
        pub fn system_clock(mut self) -> Self {
            self.builder = self.builder.system_clock();
            self
        }

        /// Register a host function callable from Python by bare name.
        /// See [`MontyExecutorBuilder::function`].
        #[must_use]
        pub fn function(mut self, function: Arc<dyn HostFunction>) -> Self {
            self.builder = self.builder.function(function);
            self
        }

        /// Register a closure as a host function.
        /// See [`MontyExecutorBuilder::function_fn`].
        #[must_use]
        pub fn function_fn<F, Fut>(
            mut self,
            name: impl Into<String>,
            description: impl Into<String>,
            func: F,
        ) -> Self
        where
            F: Fn(Vec<Value>, Map<String, Value>) -> Fut + Send + Sync + 'static,
            Fut: Future<Output = std::result::Result<Value, HostFunctionError>> + Send + 'static,
        {
            self.builder = self.builder.function_fn(name, description, func);
            self
        }

        /// Cap the interpreter's memory use in bytes.
        /// See [`MontyExecutorBuilder::max_memory`].
        #[must_use]
        pub fn max_memory(mut self, bytes: usize) -> Self {
            self.builder = self.builder.max_memory(bytes);
            self
        }

        /// Wall-clock bound for a single host-function call.
        /// See [`MontyExecutorBuilder::host_function_timeout`].
        #[must_use]
        pub fn host_function_timeout(mut self, timeout: Duration) -> Self {
            self.builder = self.builder.host_function_timeout(timeout);
            self
        }

        /// Cap on concurrently retained REPL sessions (default 100). The
        /// least recently used session is evicted at the cap; its next call
        /// starts a fresh interpreter. A value of 0 is treated as 1.
        #[must_use]
        pub fn max_sessions(mut self, max_sessions: usize) -> Self {
            self.max_sessions = max_sessions;
            self
        }

        /// Build a one-shot tool: each call runs in a fresh interpreter.
        ///
        /// # Errors
        ///
        /// Returns a [`MontyBuildError`] when the host-function registry is
        /// invalid.
        pub fn build_one_shot(self) -> std::result::Result<MontyPythonCodeTool, MontyBuildError> {
            let executor = Arc::new(self.builder.clone().build_one_shot()?);
            Ok(MontyPythonCodeTool {
                repl: false,
                description: compose_description(executor.prompt_snippet()),
                inner: Inner {
                    builder: self.builder,
                    granted: executor.granted_policy(),
                    oneshot: Some(executor),
                    sessions: RwLock::new(LruSessions::new(self.max_sessions)),
                },
            })
        }

        /// Build a REPL tool: interpreter state persists across calls, scoped
        /// per ADK session.
        ///
        /// # Errors
        ///
        /// Returns a [`MontyBuildError`] when the host-function registry is
        /// invalid.
        pub fn build_repl(self) -> std::result::Result<MontyPythonCodeTool, MontyBuildError> {
            // A probe executor renders the snippet; all sessions share the
            // same grants and registry, so the description is identical
            // across sessions and stays valid.
            let probe = self.builder.clone().build_repl()?;
            Ok(MontyPythonCodeTool {
                repl: true,
                description: compose_description(probe.prompt_snippet()),
                inner: Inner {
                    builder: self.builder,
                    granted: probe.granted_policy(),
                    oneshot: None,
                    sessions: RwLock::new(LruSessions::new(self.max_sessions)),
                },
            })
        }
    }

    fn compose_description(snippet: Option<String>) -> String {
        match snippet {
            Some(snippet) => format!("{BASE_DESCRIPTION}\n\n{snippet}"),
            None => BASE_DESCRIPTION.to_string(),
        }
    }

    impl MontyPythonCodeTool {
        /// The live execution path: dispatch to the shared one-shot executor,
        /// or to the calling session's REPL executor.
        pub(super) async fn execute_python(
            &self,
            ctx: Arc<dyn ToolContext>,
            code: &str,
            input: Option<Value>,
            timeout_secs: u64,
            reset: bool,
        ) -> Result<Value> {
            let executor: Arc<dyn CodeExecutor> = if let Some(oneshot) = &self.inner.oneshot {
                oneshot.clone()
            } else {
                match self.repl_executor(SessionKey::new(ctx.as_ref()), reset).await {
                    Ok(executor) => executor,
                    Err(err) => return Ok(render_error(err)),
                }
            };

            // Request exactly the tool's grants; the model controls only the
            // code, the input, and the time budget.
            let mut sandbox = self.inner.granted.clone();
            sandbox.timeout = Duration::from_secs(timeout_secs);

            let request = ExecutionRequest {
                language: ExecutionLanguage::Python,
                payload: ExecutionPayload::Source { code: code.to_string() },
                argv: vec![],
                stdin: None,
                input,
                sandbox,
                identity: None,
            };

            match executor.execute(request).await {
                Ok(result) => Ok(render_result(result)),
                Err(err) => Ok(render_error(err)),
            }
        }

        /// Get (or lazily create) the REPL executor for `key`; `reset`
        /// discards any existing session first. A build failure is an
        /// [`ExecutionError`] so the caller renders it as envelope data,
        /// never a `ToolError`.
        async fn repl_executor(
            &self,
            key: SessionKey,
            reset: bool,
        ) -> std::result::Result<Arc<MontyReplExecutor>, ExecutionError> {
            let mut sessions = self.inner.sessions.write().await;
            if reset {
                sessions.remove(&key);
            }
            if let Some(executor) = sessions.get(&key) {
                return Ok(executor);
            }
            let executor = Arc::new(self.inner.builder.clone().build_repl().map_err(|err| {
                ExecutionError::InternalError(format!("failed to build repl session: {err}"))
            })?);
            debug!(
                session.app = %key.app,
                session.user = %key.user,
                session.id = %key.session,
                "created repl session"
            );
            sessions.insert(key, executor.clone());
            Ok(executor)
        }

        /// Whether a REPL session currently exists for the calling context's
        /// session identity (test support).
        #[cfg(test)]
        pub(super) async fn has_session(&self, ctx: &dyn ToolContext) -> bool {
            self.inner.sessions.read().await.contains(&SessionKey::new(ctx))
        }
    }

    // Execution is in-process — no process is spawned, so the envelope has no
    // `exitCode` (unlike the container-backed code tools). `status` is the
    // success/failure signal.
    fn render_result(result: ExecutionResult) -> Value {
        json!({
            "status": result.status,
            "stdout": result.stdout,
            "stderr": result.stderr,
            "output": result.output,
            "stdoutTruncated": result.stdout_truncated,
            "stderrTruncated": result.stderr_truncated,
            "durationMs": result.duration_ms,
        })
    }

    /// Executor-level errors (policy, internal) are information for the
    /// model, never a `ToolError`.
    fn render_error(err: ExecutionError) -> Value {
        let status = match err {
            ExecutionError::InvalidRequest(_)
            | ExecutionError::UnsupportedPolicy(_)
            | ExecutionError::Rejected(_) => "rejected",
            ExecutionError::Timeout(_) => "timeout",
            ExecutionError::UnsupportedLanguage(_)
            | ExecutionError::CompileFailed(_)
            | ExecutionError::ExecutionFailed(_)
            | ExecutionError::InternalError(_) => "failed",
        };
        json!({
            "status": status,
            "stdout": "",
            "stderr": err.to_string(),
            "output": null,
            "stdoutTruncated": false,
            "stderrTruncated": false,
            "durationMs": 0,
        })
    }
}

/// Fallback when the `code-embedded-python` feature is not enabled.
#[cfg(not(feature = "code-embedded-python"))]
impl MontyPythonCodeTool {
    pub(super) async fn execute_python(
        &self,
        _ctx: Arc<dyn ToolContext>,
        _code: &str,
        _input: Option<Value>,
        _timeout_secs: u64,
        _reset: bool,
    ) -> Result<Value> {
        Ok(rejected(
            "Python execution requires the 'code-embedded-python' feature. \
             Enable it with: adk-tool = { features = [\"code-embedded-python\"] }",
        ))
    }
}

#[cfg(all(test, feature = "code-embedded-python"))]
mod tests {
    use super::*;
    use adk_core::{CallbackContext, Content, EventActions, ReadonlyContext};
    use std::sync::Mutex;

    struct MockToolContext {
        user: String,
        session: String,
        actions: Mutex<EventActions>,
        content: Content,
    }

    impl MockToolContext {
        fn new(session: &str) -> Arc<Self> {
            Self::for_user("user-1", session)
        }

        fn for_user(user: &str, session: &str) -> Arc<Self> {
            Arc::new(Self {
                user: user.to_string(),
                session: session.to_string(),
                actions: Mutex::new(EventActions::default()),
                content: Content::new("user"),
            })
        }
    }

    #[async_trait]
    impl ReadonlyContext for MockToolContext {
        fn invocation_id(&self) -> &str {
            "inv-test"
        }
        fn agent_name(&self) -> &str {
            "test-agent"
        }
        fn user_id(&self) -> &str {
            &self.user
        }
        fn app_name(&self) -> &str {
            "test-app"
        }
        fn session_id(&self) -> &str {
            &self.session
        }
        fn branch(&self) -> &str {
            ""
        }
        fn user_content(&self) -> &Content {
            &self.content
        }
    }

    #[async_trait]
    impl CallbackContext for MockToolContext {
        fn artifacts(&self) -> Option<Arc<dyn adk_core::Artifacts>> {
            None
        }
    }

    #[async_trait]
    impl ToolContext for MockToolContext {
        fn function_call_id(&self) -> &str {
            "call-test"
        }
        fn actions(&self) -> EventActions {
            self.actions.lock().unwrap().clone()
        }
        fn set_actions(&self, actions: EventActions) {
            *self.actions.lock().unwrap() = actions;
        }
        async fn search_memory(&self, _query: &str) -> Result<Vec<adk_core::MemoryEntry>> {
            Ok(vec![])
        }
    }

    #[test]
    fn schema_includes_reset_only_in_repl_mode() {
        let one_shot = MontyPythonCodeTool::new();
        let schema = one_shot.parameters_schema().unwrap();
        assert!(schema["properties"]["code"].is_object());
        assert!(schema["properties"]["timeout_secs"].is_object());
        assert!(schema["properties"].get("reset").is_none());
        assert_eq!(schema["required"], json!(["code"]));

        let repl = MontyPythonCodeTool::repl();
        let schema = repl.parameters_schema().unwrap();
        assert!(schema["properties"]["reset"].is_object());
    }

    #[test]
    fn description_is_base_contract_plus_prompt_snippet() {
        let tool = MontyPythonCodeTool::builder()
            .environ_var("PROJECT", "secret-value")
            .function_fn("noop", "Do nothing.", |_args, _kwargs| async move { Ok(json!(null)) })
            .build_repl()
            .unwrap();
        let description = tool.description();
        assert!(description.starts_with(BASE_DESCRIPTION));
        assert!(description.contains("## Python execution environment"));
        assert!(description.contains("persistent REPL session"));
        assert!(description.contains("PROJECT"));
        assert!(!description.contains("secret-value"));
        assert!(description.contains("def noop(...):"));
    }

    #[tokio::test]
    async fn response_schema_matches_the_actual_envelope() {
        let tool = MontyPythonCodeTool::new();
        let schema = tool.response_schema().expect("the envelope is fixed, so it is declared");

        // Every declared property is required, and the live envelope carries
        // exactly those keys — in success, failure, and rejection shapes.
        let mut declared: Vec<&str> =
            schema["properties"].as_object().unwrap().keys().map(String::as_str).collect();
        let mut required: Vec<&str> =
            schema["required"].as_array().unwrap().iter().filter_map(Value::as_str).collect();
        declared.sort_unstable();
        required.sort_unstable();
        assert_eq!(declared, required);

        for args in [json!({"code": "21 * 2"}), json!({"code": "1 / 0"}), json!({})] {
            let result = tool.execute(MockToolContext::new("s1"), args).await.unwrap();
            let mut actual: Vec<&str> =
                result.as_object().unwrap().keys().map(String::as_str).collect();
            actual.sort_unstable();
            assert_eq!(actual, declared);

            let status = result["status"].as_str().unwrap();
            let allowed: Vec<&str> = schema["properties"]["status"]["enum"]
                .as_array()
                .unwrap()
                .iter()
                .filter_map(Value::as_str)
                .collect();
            assert!(allowed.contains(&status), "undeclared status: {status}");
        }
    }

    #[test]
    fn declaration_includes_parameters_and_response() {
        let tool = MontyPythonCodeTool::new();
        let declaration = tool.declaration();
        assert_eq!(declaration["name"], "monty_python_code");
        assert!(declaration["parameters"].is_object());
        assert!(declaration["response"].is_object());
    }

    #[test]
    fn concurrency_metadata_tracks_the_mode() {
        assert!(MontyPythonCodeTool::new().is_concurrency_safe());
        assert!(!MontyPythonCodeTool::repl().is_concurrency_safe());
        assert!(!MontyPythonCodeTool::new().is_read_only());
    }

    #[tokio::test]
    async fn missing_code_is_rejected() {
        let tool = MontyPythonCodeTool::new();
        let result = tool.execute(MockToolContext::new("s1"), json!({})).await.unwrap();
        assert_eq!(result["status"], "rejected");
        assert!(result["stderr"].as_str().unwrap().contains("code"));
    }

    #[tokio::test]
    async fn one_shot_executes_and_reports_camel_case_envelope() {
        let tool = MontyPythonCodeTool::new();
        let result =
            tool.execute(MockToolContext::new("s1"), json!({"code": "21 * 2"})).await.unwrap();
        assert_eq!(result["status"], "success");
        assert_eq!(result["output"], json!(42));
        assert!(result.get("exitCode").is_none());
        assert!(result.get("durationMs").is_some());
        assert_eq!(result["stdoutTruncated"], json!(false));
        assert_eq!(result["stderrTruncated"], json!(false));
    }

    #[tokio::test]
    async fn python_exception_reports_failed_with_traceback() {
        let tool = MontyPythonCodeTool::new();
        let result =
            tool.execute(MockToolContext::new("s1"), json!({"code": "1 / 0"})).await.unwrap();
        assert_eq!(result["status"], "failed");
        assert!(result["stderr"].as_str().unwrap().contains("ZeroDivisionError"));
    }

    #[tokio::test]
    async fn repl_sessions_are_isolated_per_session_id() {
        let tool = MontyPythonCodeTool::repl();

        tool.execute(MockToolContext::new("alice"), json!({"code": "x = 1"})).await.unwrap();

        // Bob's session has no `x`.
        let bob = tool.execute(MockToolContext::new("bob"), json!({"code": "x"})).await.unwrap();
        assert_eq!(bob["status"], "failed");
        assert!(bob["stderr"].as_str().unwrap().contains("NameError"));

        // Alice's session still does.
        let alice =
            tool.execute(MockToolContext::new("alice"), json!({"code": "x + 1"})).await.unwrap();
        assert_eq!(alice["output"], json!(2));
    }

    #[tokio::test]
    async fn same_session_id_for_different_users_gets_isolated_interpreters() {
        let tool = MontyPythonCodeTool::repl();

        // Two users independently chose the session id "s1".
        let alice = MockToolContext::for_user("alice", "s1");
        let bob = MockToolContext::for_user("bob", "s1");

        tool.execute(alice.clone(), json!({"code": "x = 1"})).await.unwrap();

        // Bob's interpreter has no `x` — the key is the full session identity.
        let result = tool.execute(bob.clone(), json!({"code": "x"})).await.unwrap();
        assert_eq!(result["status"], "failed");
        assert!(result["stderr"].as_str().unwrap().contains("NameError"));
        assert!(tool.has_session(alice.as_ref()).await);
        assert!(tool.has_session(bob.as_ref()).await);
    }

    #[tokio::test]
    async fn reset_true_clears_the_session_state() {
        let tool = MontyPythonCodeTool::repl();
        let ctx = MockToolContext::new("s1");

        tool.execute(ctx.clone(), json!({"code": "x = 1"})).await.unwrap();
        let result = tool.execute(ctx, json!({"code": "x", "reset": true})).await.unwrap();
        assert_eq!(result["status"], "failed");
        assert!(result["stderr"].as_str().unwrap().contains("NameError"));
    }

    #[tokio::test]
    async fn lru_evicts_the_least_recently_used_session() {
        let tool = MontyPythonCodeTool::builder().max_sessions(2).build_repl().unwrap();

        tool.execute(MockToolContext::new("a"), json!({"code": "x = 1"})).await.unwrap();
        tool.execute(MockToolContext::new("b"), json!({"code": "x = 2"})).await.unwrap();
        // Touch `a` so `b` becomes the LRU entry.
        tool.execute(MockToolContext::new("a"), json!({"code": "x"})).await.unwrap();
        // A third session evicts `b`.
        tool.execute(MockToolContext::new("c"), json!({"code": "x = 3"})).await.unwrap();

        assert!(tool.has_session(MockToolContext::new("a").as_ref()).await);
        assert!(!tool.has_session(MockToolContext::new("b").as_ref()).await);
        assert!(tool.has_session(MockToolContext::new("c").as_ref()).await);

        // The evicted session's next call transparently starts fresh.
        let result = tool.execute(MockToolContext::new("b"), json!({"code": "x"})).await.unwrap();
        assert_eq!(result["status"], "failed");
    }

    #[tokio::test]
    async fn granted_input_and_host_function_flow_through_the_tool() {
        let tool = MontyPythonCodeTool::builder()
            .environ_var("PROJECT", "acme")
            .function_fn("double", "Double a number.", |args, _kwargs| async move {
                let n = args.first().and_then(Value::as_i64).unwrap_or(0);
                Ok(json!(n * 2))
            })
            .build_one_shot()
            .unwrap();

        let result = tool
            .execute(
                MockToolContext::new("s1"),
                json!({
                    "code": "import os\n[double(input['n']), os.getenv('PROJECT')]",
                    "input": {"n": 21}
                }),
            )
            .await
            .unwrap();
        assert_eq!(result["status"], "success");
        assert_eq!(result["output"], json!([42, "acme"]));
    }
}
