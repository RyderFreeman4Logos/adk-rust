//! The [`SandboxBackend`] trait and [`BackendCapabilities`] descriptor.

use async_trait::async_trait;
use std::sync::Arc;

use crate::error::SandboxError;
use crate::types::{ExecRequest, ExecResult, Language};

/// Async trait for isolated code execution backends.
///
/// Implementations provide a single `execute()` method that runs code in
/// isolation and returns the result. The trait is intentionally minimal —
/// no lifecycle methods (start/stop/restart). `ProcessBackend` is stateless
/// and `WasmBackend` creates a fresh instance per call.
///
/// # Example
///
/// ```rust,ignore
/// use adk_sandbox::{SandboxBackend, ExecRequest, ExecResult, SandboxError};
///
/// struct MyBackend;
///
/// #[async_trait::async_trait]
/// impl SandboxBackend for MyBackend {
///     fn name(&self) -> &str { "my-backend" }
///     fn capabilities(&self) -> BackendCapabilities { /* ... */ }
///     async fn execute(&self, request: ExecRequest) -> Result<ExecResult, SandboxError> {
///         // Execute code in isolation
///         todo!()
///     }
/// }
/// ```
#[async_trait]
pub trait SandboxBackend: Send + Sync {
    /// Returns the backend name (e.g., `"process"`, `"wasm"`).
    fn name(&self) -> &str;

    /// Returns the capabilities and enforced limits of this backend.
    fn capabilities(&self) -> BackendCapabilities;

    /// Executes code in isolation according to the request parameters.
    ///
    /// # Errors
    ///
    /// Returns [`SandboxError`] if execution cannot complete (timeout,
    /// memory exceeded, invalid request, etc.). A non-zero exit code
    /// is **not** an error — it is returned in [`ExecResult::exit_code`].
    async fn execute(&self, request: ExecRequest) -> Result<ExecResult, SandboxError>;

    /// Execute while retaining caller-owned artifacts until this backend stops
    /// using them. The default keeps the in-flight backend future alive after
    /// caller cancellation and releases only on its natural return. Backends
    /// remain responsible for their own timeout and settlement contract.
    /// ProcessBackend overrides this to transfer custody into its physical owner.
    async fn execute_with_keepalive(
        self: Arc<Self>,
        request: ExecRequest,
        keepalive: Arc<dyn Send + Sync>,
    ) -> Result<ExecResult, SandboxError>
    where
        Self: 'static,
    {
        tokio::spawn(async move {
            let _keepalive = keepalive;
            self.execute(request).await
        })
        .await
        .map_err(|error| SandboxError::ExecutionFailed(format!("backend owner failed: {error}")))?
    }
}

/// Describes what a backend supports and enforces.
///
/// Callers can inspect capabilities to choose the right backend or to
/// understand what isolation guarantees are provided.
///
/// # Example
///
/// ```rust
/// use adk_sandbox::{BackendCapabilities, EnforcedLimits, Language};
///
/// let caps = BackendCapabilities {
///     supported_languages: vec![Language::Python, Language::JavaScript],
///     isolation_class: "process".to_string(),
///     enforced_limits: EnforcedLimits {
///         timeout: true,
///         memory: false,
///         network_isolation: false,
///         filesystem_write_isolation: false,
///         filesystem_read_isolation: false,
///         environment_isolation: true,
///     },
/// };
/// assert!(caps.enforced_limits.timeout);
/// ```
#[derive(Debug, Clone)]
pub struct BackendCapabilities {
    /// Languages this backend can execute.
    pub supported_languages: Vec<Language>,
    /// Isolation class identifier (e.g., `"process"`, `"wasm"`, `"container"`).
    pub isolation_class: String,
    /// Which resource limits the backend actually enforces.
    pub enforced_limits: EnforcedLimits,
}

/// Describes which resource limits a backend enforces.
///
/// Backends are honest about what they enforce. For example,
/// `ProcessBackend` enforces timeout and environment isolation
/// but not memory or network isolation.
#[derive(Debug, Clone)]
pub struct EnforcedLimits {
    /// Whether the backend enforces execution timeout.
    pub timeout: bool,
    /// Whether the backend enforces memory limits.
    pub memory: bool,
    /// Whether the backend isolates network access.
    pub network_isolation: bool,
    /// Whether the backend prevents writes outside the policy's allowed paths.
    pub filesystem_write_isolation: bool,
    /// Whether the backend prevents *reads* outside the policy's allowed paths.
    ///
    /// Separate from write isolation because they are not equivalent and the platforms
    /// differ. Linux bubblewrap mounts only the allowed paths, and the macOS Seatbelt
    /// profile is deny-by-default with read access to the system runtime added, so both
    /// report it. A backend without an OS enforcer does not.
    pub filesystem_read_isolation: bool,
    /// Whether the backend isolates environment variables.
    pub environment_isolation: bool,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::sync::Mutex;
    use std::time::Duration;
    use tokio::sync::oneshot;

    struct PendingBackend {
        started: Mutex<Option<oneshot::Sender<()>>>,
        release: Mutex<Option<oneshot::Receiver<()>>>,
    }

    #[async_trait]
    impl SandboxBackend for PendingBackend {
        fn name(&self) -> &str {
            "pending-fixture"
        }

        fn capabilities(&self) -> BackendCapabilities {
            BackendCapabilities {
                supported_languages: vec![Language::Command],
                isolation_class: "fixture".to_string(),
                enforced_limits: EnforcedLimits {
                    timeout: false,
                    memory: false,
                    network_isolation: false,
                    filesystem_write_isolation: false,
                    filesystem_read_isolation: false,
                    environment_isolation: false,
                },
            }
        }

        async fn execute(&self, _: ExecRequest) -> Result<ExecResult, SandboxError> {
            let release =
                self.release.lock().expect("fixture release lock").take().expect("release");
            let started = self.started.lock().expect("fixture ready lock").take().expect("started");
            let _ = started.send(());
            release.await.expect("release backend naturally");
            Ok(ExecResult {
                stdout: String::new(),
                stderr: String::new(),
                exit_code: 0,
                duration: Duration::ZERO,
            })
        }
    }

    struct DropNotice(Option<oneshot::Sender<()>>);

    impl Drop for DropNotice {
        fn drop(&mut self) {
            if let Some(notice) = self.0.take() {
                let _ = notice.send(());
            }
        }
    }

    #[tokio::test]
    async fn default_backend_retains_caller_artifact_until_backend_actually_returns() {
        let (started, ready) = oneshot::channel();
        let (release, released) = oneshot::channel();
        let (dropped, mut drop_notice) = oneshot::channel();
        let backend: Arc<dyn SandboxBackend> = Arc::new(PendingBackend {
            started: Mutex::new(Some(started)),
            release: Mutex::new(Some(released)),
        });
        let caller = tokio::spawn(backend.execute_with_keepalive(
            ExecRequest {
                language: Language::Command,
                code: String::new(),
                stdin: None,
                timeout: Duration::from_secs(10),
                memory_limit_mb: None,
                env: HashMap::new(),
            },
            Arc::new(DropNotice(Some(dropped))),
        ));
        ready.await.expect("backend has started using artifact");
        caller.abort();
        assert!(caller.await.expect_err("cancel caller").is_cancelled());
        assert!(matches!(drop_notice.try_recv(), Err(oneshot::error::TryRecvError::Empty)));
        release.send(()).expect("backend still in flight after caller cancellation");
        tokio::time::timeout(Duration::from_secs(5), &mut drop_notice)
            .await
            .expect("artifact release watchdog")
            .expect("artifact released after natural backend return");
    }
}
