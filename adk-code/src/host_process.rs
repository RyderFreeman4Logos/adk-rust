//! Host-process plumbing shared by the executors that spawn local processes.
//!
//! Rust processes configured here begin from a cleared environment. `rustc`
//! receives only the toolchain allowlist from [`ProcessBackend::toolchain_env`],
//! so compile-time `env!` and `option_env!` cannot bake host credentials into a
//! binary, and compiled binaries receive only the variables their
//! [`EnvironmentPolicy`] names.

use std::ffi::OsString;
use std::future::Future;
use std::io;
use std::process::{ExitStatus, Output};
use std::time::Duration;

use adk_sandbox::ProcessBackend;
use adk_sandbox::process_custody::{CompletionBoundary, ProcessIo, run_owned_command};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::process::Command;

use crate::EnvironmentPolicy;

/// Returns the host variables `policy` exposes, skipping names the host does not set.
pub(crate) fn allowlisted_host_env(policy: &EnvironmentPolicy) -> Vec<(String, OsString)> {
    match policy {
        EnvironmentPolicy::None => Vec::new(),
        EnvironmentPolicy::AllowList(names) => names
            .iter()
            .filter_map(|name| std::env::var_os(name).map(|value| (name.clone(), value)))
            .collect(),
    }
}

/// Returns the environment for a compiled user binary.
///
/// Windows binaries also receive `SystemRoot`, which system DLL initialisation
/// (Winsock, for example) reads; it names the OS directory and carries no secret.
pub(crate) fn binary_env(policy: &EnvironmentPolicy) -> Vec<(String, OsString)> {
    let environment = allowlisted_host_env(policy);

    #[cfg(windows)]
    let mut environment = environment;

    #[cfg(windows)]
    if !environment.iter().any(|(key, _)| key.eq_ignore_ascii_case("SystemRoot"))
        && let Some(value) = std::env::var_os("SystemRoot")
    {
        environment.push(("SystemRoot".to_string(), value));
    }

    environment
}

/// Builds a `rustc` command that sees only the toolchain environment.
///
/// Pass the configured command to `run_compiler`; its physical owner retains
/// the compiler group and artifacts after timeout or caller cancellation.
pub(crate) fn rustc_command(rustc_path: &str) -> Command {
    let mut command = Command::new(rustc_path);
    command.env_clear().envs(ProcessBackend::toolchain_env());
    // Cargo applies the workspace's rust-lld setting, but a direct rustc invocation does
    // not read .cargo/config.toml. Hosted Windows runners put Git's GNU `link.exe` ahead
    // of MSVC on PATH, so naming rust-lld keeps rustc from launching the unrelated Unix
    // utility, as adk-sandbox's ProcessBackend does.
    #[cfg(windows)]
    command.arg("-Clinker=rust-lld");
    command
}

/// Bytes captured from one output stream.
#[derive(Debug, Default)]
pub(crate) struct CapturedStream {
    /// At most the caller's cap.
    pub(crate) bytes: Vec<u8>,
    /// Whether output past the cap was read and discarded.
    pub(crate) truncated: bool,
}

/// A child process that ran to completion.
#[derive(Debug)]
pub(crate) struct CompletedProcess {
    pub(crate) status: ExitStatus,
    pub(crate) stdout: CapturedStream,
    pub(crate) stderr: CapturedStream,
}

/// Collect a compiler exactly like locked Tokio wait_with_output: wait/stdout/
/// stderr errors fail fast in that fixed poll order; inherited stdin is complete.
/// The original allowance starts after successful synchronous spawn. Timeout is
/// returned only after settlement; incomplete cleanup is a distinct I/O error.
pub(crate) async fn run_compiler<K>(
    command: Command,
    timeout: Duration,
    keepalive: K,
) -> io::Result<Option<Output>>
where
    K: Send + 'static,
{
    run_owned_command(
        command,
        timeout,
        keepalive,
        CompletionBoundary::OutputEof,
        |pipes| async move {
            let ProcessIo { stdin, stdout, stderr, status } = pipes;
            drop(stdin);
            let wait = async { status.await.map_err(io::Error::other)? };
            collect_compiler(wait, read_all(stdout), read_all(stderr)).await
        },
    )
    .await
}

async fn collect_compiler<S, O, E>(status: S, stdout: O, stderr: E) -> io::Result<Output>
where
    S: Future<Output = io::Result<ExitStatus>>,
    O: Future<Output = io::Result<Vec<u8>>>,
    E: Future<Output = io::Result<Vec<u8>>>,
{
    // Locked Tokio's private try_join3 always polls in this order, including
    // after a prior pending poll. The default rotating macro is not equivalent.
    let (status, stdout, stderr) = tokio::try_join!(biased; status, stdout, stderr)?;
    Ok(Output { status, stdout, stderr })
}

async fn read_all<R: AsyncRead + Unpin>(pipe: Option<R>) -> io::Result<Vec<u8>> {
    let mut bytes = Vec::new();
    if let Some(mut pipe) = pipe {
        pipe.read_to_end(&mut bytes).await?;
    }
    Ok(bytes)
}

/// A legacy host failure, with only the diagnostic boundary observed by the
/// collector callback. This marker is not evidence of physical settlement.
#[derive(Debug)]
pub(crate) struct ProcessFailure {
    error: io::Error,
    collection_started: bool,
}

impl ProcessFailure {
    fn observed(error: io::Error, started: &mut tokio::sync::oneshot::Receiver<()>) -> Self {
        Self { error, collection_started: started.try_recv().is_ok() }
    }

    pub(crate) fn context(self, spawn: &str, completion: &str) -> String {
        let context = if self.collection_started { completion } else { spawn };
        format!("{context}: {}", self.error)
    }
}

/// Feed input while draining capped output and observing leader completion.
/// All four futures finish before propagating errors in stdin/wait/stdout/stderr
/// order. BrokenPipe remains nonfatal. Timeout/cancellation owns physical cleanup
/// and the supplied artifacts; no caller-owned child is released between spawn
/// and custody. The work allowance remains one post-spawn duration.
pub(crate) async fn run_to_completion<K>(
    command: Command,
    stdin: Option<Vec<u8>>,
    timeout: Duration,
    stdout_cap: usize,
    stderr_cap: usize,
    keepalive: K,
) -> Result<Option<CompletedProcess>, ProcessFailure>
where
    K: Send + 'static,
{
    let (collection_started, mut started) = tokio::sync::oneshot::channel();
    run_owned_command(command, timeout, keepalive, CompletionBoundary::OutputEof, move |pipes| {
        let _ = collection_started.send(());
        async move {
            collect_completion(
                write_stdin(pipes.stdin, stdin),
                async { pipes.status.await.map_err(io::Error::other)? },
                read_capped(pipes.stdout, stdout_cap),
                read_capped(pipes.stderr, stderr_cap),
            )
            .await
        }
    })
    .await
    .map_err(|error| ProcessFailure::observed(error, &mut started))
}

async fn write_stdin<W: AsyncWrite + Unpin>(
    pipe: Option<W>,
    bytes: Option<Vec<u8>>,
) -> io::Result<()> {
    if let (Some(mut pipe), Some(bytes)) = (pipe, bytes) {
        match pipe.write_all(&bytes).await {
            Err(error) if error.kind() == io::ErrorKind::BrokenPipe => {}
            result => result?,
        }
    }
    Ok(())
}

async fn collect_completion<F, S, O, E>(
    feed: F,
    status: S,
    stdout: O,
    stderr: E,
) -> io::Result<CompletedProcess>
where
    F: Future<Output = io::Result<()>>,
    S: Future<Output = io::Result<ExitStatus>>,
    O: Future<Output = io::Result<CapturedStream>>,
    E: Future<Output = io::Result<CapturedStream>>,
{
    let (fed, stdout, stderr, status) = tokio::join!(feed, stdout, stderr, status);
    fed?;
    Ok(CompletedProcess { status: status?, stdout: stdout?, stderr: stderr? })
}

/// Reads `pipe` to EOF, retaining at most `cap` bytes.
///
/// Reading continues past the cap because an undrained pipe blocks the child
/// until the deadline.
async fn read_capped<R>(pipe: Option<R>, cap: usize) -> io::Result<CapturedStream>
where
    R: AsyncRead + Unpin,
{
    let mut captured = CapturedStream::default();
    let Some(mut pipe) = pipe else {
        return Ok(captured);
    };

    let mut chunk = [0u8; 8192];
    loop {
        let read = pipe.read(&mut chunk).await?;
        if read == 0 {
            return Ok(captured);
        }
        let take = cap.saturating_sub(captured.bytes.len()).min(read);
        captured.bytes.extend_from_slice(&chunk[..take]);
        captured.truncated |= take < read;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Stdio;
    use std::time::Instant;

    struct FailingWriter {
        ready: Option<tokio::sync::oneshot::Sender<()>>,
        kind: io::ErrorKind,
    }

    impl AsyncWrite for FailingWriter {
        fn poll_write(
            mut self: std::pin::Pin<&mut Self>,
            _: &mut std::task::Context<'_>,
            _: &[u8],
        ) -> std::task::Poll<io::Result<usize>> {
            if let Some(ready) = self.ready.take() {
                let _ = ready.send(());
            }
            std::task::Poll::Ready(Err(io::Error::new(self.kind, "stdin infrastructure error")))
        }

        fn poll_flush(
            self: std::pin::Pin<&mut Self>,
            _: &mut std::task::Context<'_>,
        ) -> std::task::Poll<io::Result<()>> {
            std::task::Poll::Ready(Ok(()))
        }

        fn poll_shutdown(
            self: std::pin::Pin<&mut Self>,
            _: &mut std::task::Context<'_>,
        ) -> std::task::Poll<io::Result<()>> {
            std::task::Poll::Ready(Ok(()))
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn early_host_stdin_error_remains_pending_until_work_timeout() {
        let (failed, observed) = tokio::sync::oneshot::channel();
        let (_release, status) = tokio::sync::oneshot::channel::<io::Result<ExitStatus>>();
        tokio::time::pause();
        let work = tokio::spawn(tokio::time::timeout(
            Duration::from_secs(5),
            collect_completion(
                write_stdin(
                    Some(FailingWriter { ready: Some(failed), kind: io::ErrorKind::Other }),
                    Some(vec![b'x']),
                ),
                async { status.await.map_err(io::Error::other)? },
                std::future::ready(Ok(CapturedStream::default())),
                std::future::ready(Ok(CapturedStream::default())),
            ),
        ));
        observed.await.expect("actual failing AsyncWrite poll");
        assert!(!work.is_finished(), "join-all cannot publish stdin error while status is pending");
        tokio::time::advance(Duration::from_secs(6)).await;
        tokio::time::resume();
        assert!(work.await.expect("actual collector join").is_err(), "original work timeout wins");
    }

    #[tokio::test]
    async fn host_broken_pipe_is_nonfatal() {
        write_stdin(
            Some(FailingWriter { ready: None, kind: io::ErrorKind::BrokenPipe }),
            Some(vec![b'x']),
        )
        .await
        .expect("original early child stdin closure is not fatal");
    }

    #[tokio::test]
    async fn host_join_error_precedence_remains_stdin_then_wait_then_output() {
        let error = collect_completion(
            std::future::ready(Err(io::Error::other("stdin-first"))),
            std::future::ready(Err(io::Error::other("wait-second"))),
            std::future::ready(Err(io::Error::other("stdout-third"))),
            std::future::ready(Err(io::Error::other("stderr-fourth"))),
        )
        .await
        .expect_err("all futures complete with errors");
        assert_eq!(error.to_string(), "stdin-first");
        let error = collect_completion(
            std::future::ready(Ok(())),
            std::future::ready(Err(io::Error::other("wait-second"))),
            std::future::ready(Err(io::Error::other("stdout-third"))),
            std::future::ready(Err(io::Error::other("stderr-fourth"))),
        )
        .await
        .expect_err("wait error precedes output errors");
        assert_eq!(error.to_string(), "wait-second");
        for stdout_fails in [true, false] {
            let status =
                Command::new("/bin/true").status().await.expect("actual successful status");
            let error = collect_completion(
                std::future::ready(Ok(())),
                std::future::ready(Ok(status)),
                std::future::ready(if stdout_fails {
                    Err(io::Error::other("stdout-third"))
                } else {
                    Ok(CapturedStream::default())
                }),
                std::future::ready(Err(io::Error::other("stderr-fourth"))),
            )
            .await
            .expect_err("output errors after successful stdin and status");
            assert_eq!(
                error.to_string(),
                if stdout_fails { "stdout-third" } else { "stderr-fourth" }
            );
        }
    }

    #[tokio::test]
    async fn after_start_io_failure_preserves_original_completion_contexts() {
        let (collection_started, mut started) = tokio::sync::oneshot::channel();
        let mut command = Command::new("/bin/sleep");
        command.arg("30").stdout(Stdio::piped()).stderr(Stdio::piped());
        let error = run_owned_command(
            command,
            Duration::from_secs(5),
            (),
            CompletionBoundary::OutputEof,
            move |_pipes| {
                collection_started.send(()).expect("callback starts after successful spawn");
                std::future::ready(Err::<(), _>(io::Error::other("fixture IO failure")))
            },
        )
        .await
        .expect_err("actual owner settles the started child before returning IO failure");
        let failure = ProcessFailure::observed(error, &mut started);
        assert!(failure.collection_started);
        assert_eq!(
            failure.context("failed to spawn binary", "failed to run binary"),
            "failed to run binary: fixture IO failure"
        );
        let (marked, mut started) = tokio::sync::oneshot::channel();
        marked.send(()).expect("same observed callback boundary");
        let failure =
            ProcessFailure::observed(io::Error::other("fixture IO failure"), &mut started);
        assert_eq!(
            failure.context(
                "failed to spawn container runtime 'fixture'",
                "failed to wait for container"
            ),
            "failed to wait for container: fixture IO failure"
        );
    }

    #[tokio::test]
    async fn compiler_output_failure_does_not_wait_for_pending_status() {
        let mut work = Box::pin(collect_compiler(
            std::future::pending(),
            std::future::ready(Err(io::Error::other("stdout infrastructure error"))),
            std::future::pending(),
        ));
        std::future::poll_fn(|cx| {
            match work.as_mut().poll(cx) {
                std::task::Poll::Ready(Err(error)) => {
                    assert_eq!(error.to_string(), "stdout infrastructure error");
                }
                other => panic!("locked compiler aggregation must fail fast: {other:?}"),
            }
            std::task::Poll::Ready(())
        })
        .await;
    }

    #[tokio::test]
    async fn compiler_error_poll_order_stays_fixed_after_pending() {
        let (wait_ready, wait) = tokio::sync::oneshot::channel::<io::Result<ExitStatus>>();
        let (out_ready, out) = tokio::sync::oneshot::channel::<io::Result<Vec<u8>>>();
        let mut work = Box::pin(collect_compiler(
            async { wait.await.map_err(io::Error::other)? },
            async { out.await.map_err(io::Error::other)? },
            std::future::ready(Ok(Vec::new())),
        ));
        std::future::poll_fn(|cx| {
            assert!(work.as_mut().poll(cx).is_pending());
            std::task::Poll::Ready(())
        })
        .await;
        out_ready.send(Err(io::Error::other("stdout-later"))).expect("stdout pending");
        wait_ready.send(Err(io::Error::other("wait-first"))).expect("status pending");
        assert_eq!(work.await.expect_err("fixed try_join3 priority").to_string(), "wait-first");
    }

    #[test]
    fn no_environment_policy_exposes_nothing() {
        assert!(allowlisted_host_env(&EnvironmentPolicy::None).is_empty());
    }

    #[test]
    fn allowlist_exposes_only_named_variables_the_host_sets() {
        let policy = EnvironmentPolicy::AllowList(vec![
            "PATH".to_string(),
            "ADK_CODE_VARIABLE_THE_HOST_NEVER_SETS".to_string(),
        ]);
        let expected: Vec<(String, OsString)> =
            std::env::var_os("PATH").map(|path| ("PATH".to_string(), path)).into_iter().collect();
        assert_eq!(allowlisted_host_env(&policy), expected);
    }

    #[test]
    fn rustc_command_starts_from_a_cleared_environment() {
        let command = rustc_command("rustc");
        // std renders a cleared environment as an `env -i` prefix on Unix.
        #[cfg(unix)]
        assert!(format!("{:?}", command.as_std()).starts_with("env -i "), "{command:?}");

        let explicit: Vec<String> = command
            .as_std()
            .get_envs()
            .map(|(key, _)| key.to_string_lossy().into_owned())
            .collect();
        let toolchain: Vec<String> =
            ProcessBackend::toolchain_env().into_iter().map(|(key, _)| key).collect();
        assert_eq!(explicit.len(), toolchain.len());
        assert!(explicit.iter().all(|key| toolchain.contains(key)), "{explicit:?}");
    }

    /// A child that never reads stdin must not stall the caller past the deadline.
    #[cfg(unix)]
    #[tokio::test]
    async fn unread_stdin_does_not_outlive_the_deadline() {
        let mut command = Command::new("sleep");
        command.arg("30").stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped());

        let started = Instant::now();
        let outcome = run_to_completion(
            command,
            Some(vec![b'x'; 8 * 1024 * 1024]),
            Duration::from_millis(300),
            1024,
            1024,
            (),
        )
        .await
        .expect("run");

        assert!(outcome.is_none(), "expected the deadline to pass");
        assert!(started.elapsed() < Duration::from_secs(5), "took {:?}", started.elapsed());
    }

    /// A child that fills stdout before reading stdin completes instead of deadlocking.
    #[cfg(unix)]
    #[tokio::test]
    async fn stdin_and_output_are_pumped_concurrently() {
        let mut command = Command::new("sh");
        command
            .arg("-c")
            .arg("head -c 1000000 /dev/zero; cat > /dev/null; echo done >&2")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        let completed = run_to_completion(
            command,
            Some(vec![b'x'; 1_000_000]),
            Duration::from_secs(20),
            4096,
            4096,
            (),
        )
        .await
        .expect("run")
        .expect("must complete before the deadline");

        assert!(completed.status.success());
        assert_eq!(completed.stdout.bytes.len(), 4096);
        assert!(completed.stdout.truncated);
        assert_eq!(completed.stderr.bytes, b"done\n");
        assert!(!completed.stderr.truncated);
    }
}
