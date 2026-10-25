//! Child-process plumbing shared by [`ProcessBackend`](crate::ProcessBackend) and the local
//! workspace session.
//!
//! Both run untrusted commands as a child process and need the same guarantees: output is
//! read while the child runs, under a byte cap, so a chatty child can neither deadlock on a
//! full pipe nor exhaust memory; stdin is written concurrently with those reads and inside
//! the timeout; and the child's whole process group is killed both on timeout and on normal
//! exit, so background descendants do not outlive the execution.

use std::process::ExitStatus;
use std::time::Duration;

use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt};
use tokio::process::Command;

use crate::process_custody::{CompletionBoundary, ProcessIo, run_owned_command};

/// Output of a child that exited before its timeout.
#[derive(Debug)]
pub(crate) struct CapturedOutput {
    /// The child's exit status.
    pub(crate) status: ExitStatus,
    /// Retained stdout bytes, at most the configured cap.
    pub(crate) stdout: Vec<u8>,
    /// Whether stdout bytes past the cap were discarded.
    pub(crate) stdout_truncated: bool,
    /// Retained stderr bytes, at most the configured cap.
    pub(crate) stderr: Vec<u8>,
    /// Whether stderr bytes past the cap were discarded.
    pub(crate) stderr_truncated: bool,
}

/// Collects sandbox I/O while the shared owner holds the command and artifacts.
pub(crate) async fn collect_command<K: Send + 'static>(
    command: Command,
    stdin: Option<Vec<u8>>,
    timeout: Duration,
    cap: usize,
    keepalive: K,
) -> std::io::Result<Option<CapturedOutput>> {
    run_owned_command(command, timeout, keepalive, CompletionBoundary::LeaderExit, move |pipes| {
        capture_pipes(pipes, stdin, cap)
    })
    .await
}

#[cfg(test)]
async fn collect_output(
    child: tokio::process::Child,
    stdin: Option<Vec<u8>>,
    timeout: Duration,
    cap: usize,
) -> std::io::Result<Option<CapturedOutput>> {
    crate::process_custody::collect_owned_child(
        child,
        timeout,
        CompletionBoundary::LeaderExit,
        move |pipes| capture_pipes(pipes, stdin, cap),
    )
    .await
}

async fn capture_pipes(
    pipes: ProcessIo,
    stdin: Option<Vec<u8>>,
    cap: usize,
) -> std::io::Result<CapturedOutput> {
    let writer = write_stdin(pipes.stdin, stdin);
    let status = async { pipes.status.await.map_err(std::io::Error::other)? };
    let stdout = async {
        match pipes.stdout {
            Some(pipe) => read_capped(pipe, cap).await,
            None => Ok((Vec::new(), false)),
        }
    };
    let stderr = async {
        match pipes.stderr {
            Some(pipe) => read_capped(pipe, cap).await,
            None => Ok((Vec::new(), false)),
        }
    };
    capture_ordered(status, writer, stdout, stderr).await
}

// All I/O is pumped concurrently, but errors are OBSERVED in the original
// wait -> stdout -> stderr order. A completed writer never initiates shutdown.
async fn capture_ordered<S, W, O, E>(
    status: S,
    writer: W,
    stdout: O,
    stderr: E,
) -> std::io::Result<CapturedOutput>
where
    S: std::future::Future<Output = std::io::Result<ExitStatus>>,
    W: std::future::Future<Output = ()>,
    O: std::future::Future<Output = std::io::Result<(Vec<u8>, bool)>>,
    E: std::future::Future<Output = std::io::Result<(Vec<u8>, bool)>>,
{
    tokio::pin!(status, writer, stdout, stderr);
    let mut exit = None;
    let mut out = None;
    let mut err = None;
    let mut writer_done = false;
    loop {
        if let Some(status) = exit.take() {
            let status = status?;
            if let Some(stdout) = out.take() {
                let (stdout, stdout_truncated) = stdout?;
                if let Some(stderr) = err.take() {
                    let (stderr, stderr_truncated) = stderr?;
                    // The old writer was aborted at this collection boundary.
                    return Ok(CapturedOutput {
                        status,
                        stdout,
                        stdout_truncated,
                        stderr,
                        stderr_truncated,
                    });
                }
                out = Some(Ok((stdout, stdout_truncated)));
            }
            exit = Some(Ok(status));
        }
        tokio::select! {
            value = &mut status, if exit.is_none() => exit = Some(value),
            value = &mut stdout, if out.is_none() => out = Some(value),
            value = &mut stderr, if err.is_none() => err = Some(value),
            _ = &mut writer, if !writer_done => writer_done = true,
        }
    }
}

/// Writes `input` to the child's stdin and closes it.
///
/// A child that exits or closes stdin without consuming it all makes the write fail with
/// `BrokenPipe`; that is the child's choice, not an execution failure.
async fn write_stdin<W: AsyncWrite + Unpin>(pipe: Option<W>, input: Option<Vec<u8>>) {
    let (Some(mut pipe), Some(input)) = (pipe, input) else {
        return;
    };
    match pipe.write_all(&input).await {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::BrokenPipe => {
            tracing::debug!("child closed stdin before consuming all input");
        }
        Err(error) => tracing::warn!(error = %error, "failed to write child stdin"),
    }
}

/// Reads `reader` to EOF, accumulating at most `cap` bytes.
///
/// Bytes past `cap` are read and discarded rather than left in the pipe. Stopping the read
/// would block the child on a full pipe buffer and stall it until the execution timeout, so
/// the drain continues even though the data is thrown away.
///
/// Returns the retained bytes and whether anything was discarded.
pub(crate) async fn read_capped<R>(mut reader: R, cap: usize) -> std::io::Result<(Vec<u8>, bool)>
where
    R: AsyncRead + Unpin,
{
    use tokio::io::AsyncReadExt;

    let mut retained = Vec::new();
    let mut chunk = [0u8; 8192];
    let mut discarded = false;

    loop {
        let read = reader.read(&mut chunk).await?;
        if read == 0 {
            break;
        }
        let room = cap.saturating_sub(retained.len());
        if room == 0 {
            discarded = true;
            continue;
        }
        let take = room.min(read);
        retained.extend_from_slice(&chunk[..take]);
        if take < read {
            discarded = true;
        }
    }

    Ok((retained, discarded))
}

/// Truncates a byte buffer to at most `max_bytes`, ensuring the result is
/// valid UTF-8 by backing off to the nearest char boundary.
pub(crate) fn truncate_utf8(bytes: Vec<u8>, max_bytes: usize) -> String {
    if bytes.len() <= max_bytes {
        return String::from_utf8_lossy(&bytes).into_owned();
    }
    let truncated = &bytes[..max_bytes];
    // Walk backwards to find a valid UTF-8 boundary.
    let mut end = max_bytes;
    while end > 0 && std::str::from_utf8(&truncated[..end]).is_err() {
        end -= 1;
    }
    std::str::from_utf8(&bytes[..end]).unwrap_or("").to_string()
}

/// Appends a truncation notice when output was discarded.
///
/// A model that receives silently-cut output has no way to know it is incomplete, so the notice
/// travels with the data rather than only appearing in a log. Mirrors the convention in
/// adk-python's `tools/environment` toolset.
pub(crate) fn note_truncation(mut text: String, discarded: bool) -> String {
    if discarded {
        text.push_str("\n... (truncated: output exceeded the configured limit)");
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::process::Child;

    struct FailingWriter(Option<tokio::sync::oneshot::Sender<()>>);

    impl AsyncWrite for FailingWriter {
        fn poll_write(
            mut self: std::pin::Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
            _bytes: &[u8],
        ) -> std::task::Poll<std::io::Result<usize>> {
            if let Some(failed) = self.0.take() {
                let _ = failed.send(());
            }
            std::task::Poll::Ready(Err(std::io::Error::other("controlled stdin failure")))
        }

        fn poll_flush(
            self: std::pin::Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<std::io::Result<()>> {
            std::task::Poll::Ready(Ok(()))
        }

        fn poll_shutdown(
            self: std::pin::Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<std::io::Result<()>> {
            std::task::Poll::Ready(Ok(()))
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn stdin_failure_keeps_sandbox_result_barrier_pending() {
        use std::os::unix::process::ExitStatusExt;
        let (release, released) = tokio::sync::oneshot::channel();
        let (failed, failure_observed) = tokio::sync::oneshot::channel();
        let operation = tokio::spawn(capture_ordered(
            async { released.await.map_err(std::io::Error::other) },
            write_stdin(Some(FailingWriter(Some(failed))), Some(vec![1])),
            std::future::ready(Ok((b"ordinary output".to_vec(), false))),
            std::future::ready(Ok((Vec::new(), false))),
        ));
        failure_observed.await.expect("actual failing AsyncWrite polled");
        assert!(!operation.is_finished(), "stdin failure must not end sandbox collection");
        release.send(ExitStatus::from_raw(7 << 8)).expect("result barrier retained");
        let result =
            operation.await.expect("collector actual join").expect("ordinary process result");
        assert_eq!(result.status.code(), Some(7));
        assert_eq!(result.stdout, b"ordinary output");
    }

    #[tokio::test]
    async fn early_stdout_error_waits_for_leader_and_preserves_wait_error_priority() {
        let (release, released) = tokio::sync::oneshot::channel::<()>();
        let (observed, ready) = tokio::sync::oneshot::channel();
        let operation = tokio::spawn(capture_ordered(
            async {
                released.await.map_err(std::io::Error::other)?;
                Err(std::io::Error::other("wait error wins"))
            },
            std::future::ready(()),
            async {
                let _ = observed.send(());
                Err(std::io::Error::other("early stdout error"))
            },
            std::future::pending(),
        ));
        ready.await.expect("stdout error ready");
        assert!(!operation.is_finished(), "reader error must wait its original observation point");
        release.send(()).expect("leader observation barrier retained");
        let error = operation.await.expect("collector actual join").expect_err("wait failure");
        assert_eq!(error.to_string(), "wait error wins");
    }

    /// `read_capped` must retain at most `cap` bytes regardless of how much arrives.
    ///
    /// This is the property the streaming read exists for, and it is not observable from
    /// `ExecResult`: `truncate_utf8` caps the *reported* string either way, so an end-to-end
    /// test passes even when the whole stream was buffered first. Asserting on the retained
    /// buffer is what distinguishes bounded memory from a bounded report.
    #[tokio::test]
    async fn read_capped_retains_at_most_the_cap() {
        let cap = 4_096;
        // 256x the cap, so a buffering implementation would allocate 1 MiB here.
        let source = vec![b'x'; cap * 256];

        let (retained, discarded) = read_capped(&source[..], cap).await.expect("reads");

        assert_eq!(retained.len(), cap, "retained buffer must stop at the cap");
        assert!(discarded, "the overflow must be reported as discarded");
    }

    /// Everything is retained when the stream is smaller than the cap, and nothing is flagged.
    #[tokio::test]
    async fn read_capped_retains_everything_under_the_cap() {
        let source = vec![b'y'; 100];

        let (retained, discarded) = read_capped(&source[..], 4_096).await.expect("reads");

        assert_eq!(retained, source);
        assert!(!discarded);
    }

    /// A stream landing exactly on the cap is not reported as truncated.
    #[tokio::test]
    async fn read_capped_handles_the_exact_boundary() {
        let cap = 8_192;
        let source = vec![b'z'; cap];

        let (retained, discarded) = read_capped(&source[..], cap).await.expect("reads");

        assert_eq!(retained.len(), cap);
        assert!(!discarded, "reaching the cap exactly discards nothing");
    }

    #[test]
    fn test_truncate_utf8_within_limit() {
        let data = "hello world".as_bytes().to_vec();
        let result = truncate_utf8(data, 1024);
        assert_eq!(result, "hello world");
    }

    #[test]
    fn test_truncate_utf8_at_boundary() {
        // Multi-byte UTF-8: "é" is 2 bytes (0xC3 0xA9)
        let data = "café".as_bytes().to_vec(); // 5 bytes: c a f 0xC3 0xA9
        // Truncate at 4 bytes — would split the "é"
        let result = truncate_utf8(data, 4);
        assert_eq!(result, "caf");
    }

    #[cfg(unix)]
    fn piped_shell(script: &str, stdin: bool) -> Child {
        use std::os::unix::process::CommandExt;

        let mut command = tokio::process::Command::new("/bin/sh");
        command
            .arg("-c")
            .arg(script)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .stdin(if stdin { std::process::Stdio::piped() } else { std::process::Stdio::null() })
            .kill_on_drop(true);
        command.as_std_mut().process_group(0);
        command.spawn().expect("sh spawns")
    }

    /// A child that fills its stdout pipe before reading stdin must not deadlock the writer.
    ///
    /// Writing stdin to completion before starting the readers blocked on a full stdin pipe
    /// while the child blocked on a full stdout pipe, and neither wait was under the timeout.
    #[cfg(unix)]
    #[tokio::test]
    async fn stdin_is_written_concurrently_with_the_reads() {
        let input = vec![b'i'; 512 * 1_024];
        let child = piped_shell("head -c 524288 /dev/zero; wc -c", true);

        let captured = tokio::time::timeout(
            Duration::from_secs(30),
            collect_output(child, Some(input), Duration::from_secs(20), 1_024 * 1_024),
        )
        .await
        .expect("must not deadlock")
        .expect("collects")
        .expect("finishes before the timeout");

        assert!(captured.status.success());
        let text = String::from_utf8_lossy(&captured.stdout);
        assert!(text.trim_end().ends_with("524288"), "stdin was not fully delivered: {text:?}");
    }

    /// A child that never reads stdin is still bounded by the timeout.
    #[cfg(unix)]
    #[tokio::test]
    async fn an_unread_stdin_does_not_escape_the_timeout() {
        let input = vec![b'i'; 4 * 1_024 * 1_024];
        let child = piped_shell("sleep 30", true);

        let started = std::time::Instant::now();
        let captured = collect_output(child, Some(input), Duration::from_millis(300), 1_024)
            .await
            .expect("collects");

        assert!(captured.is_none(), "the sleeping child must time out");
        assert!(started.elapsed() < Duration::from_secs(10), "took {:?}", started.elapsed());
    }

    /// Background descendants are killed when the group leader exits normally.
    #[cfg(unix)]
    #[tokio::test]
    async fn background_descendants_end_with_the_execution() {
        let directory = tempfile::tempdir().unwrap();
        let marker = directory.path().join("survivor");
        let quoted = marker.to_string_lossy().replace('\'', "'\\''");
        let child = piped_shell(
            &format!(
                "nohup sh -c 'sleep 1; touch \"$0\"' '{quoted}' >/dev/null 2>&1 & echo started"
            ),
            false,
        );

        let captured = collect_output(child, None, Duration::from_secs(10), 1_024)
            .await
            .expect("collects")
            .expect("the leader exits immediately");

        assert_eq!(String::from_utf8_lossy(&captured.stdout).trim(), "started");
        tokio::time::sleep(Duration::from_millis(1_500)).await;
        assert!(!marker.exists(), "a background descendant outlived the execution");
    }
}
