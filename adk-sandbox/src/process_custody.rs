//! Cancellation-surviving custody of one command, its group and execution resources.
//!
//! Collection remains caller-defined so compiler, host and sandbox error ordering is
//! preserved. The owner alone signals and reaps; collection receives a non-reaping
//! exit notification. An actively driven Tokio runtime and descendants remaining in
//! the private group are required. Runtime destruction and group escape are outside
//! this contract.

use std::future::Future;
use std::io;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::process::ExitStatus;
use std::task::Poll;
use std::time::Duration;

use tokio::process::{Child, ChildStderr, ChildStdin, ChildStdout, Command};
use tokio::sync::oneshot;
use tokio::time::Instant;

const CLEANUP_BUDGET: Duration = Duration::from_secs(5);

/// When normal execution first starts group shutdown.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CompletionBoundary {
    /// Wait for the caller's complete collection future, including stdin.
    OutputEof,
    /// End residual group members when the direct leader exits; collection still
    /// has the original work deadline and its original error/abort ordering.
    LeaderExit,
}

/// Stdio and a non-consuming exit notification belonging to one owned command.
///
/// The collector must own all I/O futures inline and must not detach tasks using
/// these pipes. Dropping it closes unfinished I/O before resource release. The
/// exit receiver reports observation only; the physical owner performs actual wait.
pub struct ProcessIo {
    /// Configured piped stdin, if any.
    pub stdin: Option<ChildStdin>,
    /// Configured piped stdout, if any.
    pub stdout: Option<ChildStdout>,
    /// Configured piped stderr, if any.
    pub stderr: Option<ChildStderr>,
    /// The leader's observed exit result, without reaping its group identity.
    pub status: oneshot::Receiver<io::Result<ExitStatus>>,
}

/// Run a command while retaining `keepalive` through physical process settlement.
///
/// The original work budget starts once, immediately after successful spawn.
/// Dropping this future cancels work, but a single runtime-owned task continues
/// group termination, actual child wait and pipe/resource settlement. A normal
/// or timeout result is returned only after settlement. Cleanup has one new
/// five-second return budget from its first trigger; expiry/failure returns an
/// explicit error while the same owner retains resources until natural settlement.
/// No destructive group signal is sent after the leader has been reaped.
///
/// `collect` preserves the calling phase's existing I/O/error aggregation, and
/// must not create detached pipe tasks. Its returned errors initiate cleanup;
/// merely observing an individual stdin/read error need not return the collector.
pub async fn run_owned_command<K, F, W, T>(
    mut command: Command,
    timeout: Duration,
    keepalive: K,
    boundary: CompletionBoundary,
    collect: F,
) -> io::Result<Option<T>>
where
    K: Send + 'static,
    F: FnOnce(ProcessIo) -> W + Send + 'static,
    W: Future<Output = io::Result<T>> + Send + 'static,
    T: Send + 'static,
{
    #[cfg(unix)]
    command.process_group(0);
    command.kill_on_drop(true);
    let (result, receiver) = oneshot::channel();
    tokio::spawn(async move {
        let keepalive = keepalive;
        if result.is_closed() {
            return;
        }
        let signals = match exit_signals() {
            Ok(signals) => signals,
            Err(error) => {
                let _ = result.send(Err(error));
                return;
            }
        };
        let child = match command.spawn() {
            Ok(child) => child,
            Err(error) => {
                let _ = result.send(Err(error));
                return;
            }
        };
        // Command::output() in the locked Tokio also spawns synchronously before
        // timeout() constructs its clock. Launch duration must not consume work.
        let work_deadline = tokio::time::sleep(timeout).deadline();
        own_child(
            StartedCommand { child, deadline: work_deadline, signals, keepalive },
            boundary,
            collect,
            result,
        )
        .await;
    });
    receiver.await.map_err(|error| io::Error::other(format!("process owner failed: {error}")))?
}

// Retains existing private helper controls without moving production spawn back
// outside the cancellation-surviving owner.
#[cfg(test)]
pub(crate) async fn collect_owned_child<F, W, T>(
    child: Child,
    timeout: Duration,
    boundary: CompletionBoundary,
    collect: F,
) -> io::Result<Option<T>>
where
    F: FnOnce(ProcessIo) -> W + Send + 'static,
    W: Future<Output = io::Result<T>> + Send + 'static,
    T: Send + 'static,
{
    let signals = exit_signals()?;
    let deadline = tokio::time::sleep(timeout).deadline();
    let (result, receiver) = oneshot::channel();
    tokio::spawn(own_child(
        StartedCommand { child, deadline, signals, keepalive: () },
        boundary,
        collect,
        result,
    ));
    receiver.await.map_err(io::Error::other)?
}

type ResultSender<T> = oneshot::Sender<io::Result<Option<T>>>;

struct StartedCommand<K> {
    child: Child,
    deadline: Instant,
    signals: ExitSignals,
    keepalive: K,
}

enum Outcome<T> {
    Collected(io::Result<T>),
    Timeout,
    Cancelled,
}

impl<T> Outcome<T> {
    fn cause(&self) -> String {
        match self {
            Self::Collected(Ok(_)) => "normal collection".to_string(),
            Self::Collected(Err(error)) => format!("collection error: {error}"),
            Self::Timeout => "work timeout".to_string(),
            Self::Cancelled => "caller cancellation".to_string(),
        }
    }
}

async fn own_child<K, F, W, T>(
    started: StartedCommand<K>,
    boundary: CompletionBoundary,
    collect: F,
    sender: ResultSender<T>,
) where
    K: Send + 'static,
    F: FnOnce(ProcessIo) -> W + Send + 'static,
    W: Future<Output = io::Result<T>> + Send + 'static,
    T: Send + 'static,
{
    let StartedCommand { mut child, deadline, mut signals, keepalive } = started;
    let group = child.id().and_then(|id| i32::try_from(id).ok());
    let (exit_sender, status) = oneshot::channel();
    let pipes = ProcessIo {
        stdin: child.stdin.take(),
        stdout: child.stdout.take(),
        stderr: child.stderr.take(),
        status,
    };
    let mut exit_sender = Some(exit_sender);
    let mut cleanup_deadline = None;
    let mut signal_error = None;
    let mut signalled = false;
    let mut sender = Some(sender);
    let outcome = match catch_unwind(AssertUnwindSafe(|| collect(pipes))) {
        Err(_) => {
            cleanup_deadline = Some(Instant::now() + CLEANUP_BUDGET);
            Outcome::Collected(Err(io::Error::other("collector panicked")))
        }
        Ok(work) => {
            let mut work = Box::pin(work);
            let mut leader = Box::pin(observe_exit(group, &mut signals));
            let mut leader_pending = true;
            let outcome = loop {
                // Like Tokio timeout(), poll ready collection before its timer.
                // The observer must run first so it can supply the status input.
                tokio::select! {
                    biased;
                    observed = &mut leader, if leader_pending => {
                        leader_pending = false;
                        if boundary == CompletionBoundary::LeaderExit && observed.is_ok() {
                            cleanup_deadline = Some(tokio::time::sleep(CLEANUP_BUDGET).deadline());
                            signal_error = signal_owned_group(group).err();
                            signalled = true;
                        }
                        if let Some(exit_sender) = exit_sender.take() {
                            let _ = exit_sender.send(observed);
                        }
                    }
                    collected = std::future::poll_fn(|cx| {
                        match catch_unwind(AssertUnwindSafe(|| work.as_mut().poll(cx))) {
                            Ok(polled) => polled,
                            Err(_) => Poll::Ready(Err(io::Error::other("collector panicked"))),
                        }
                    }) => break Outcome::Collected(collected),
                    _ = caller_closed(&mut sender) => break Outcome::Cancelled,
                    _ = tokio::time::sleep_until(deadline) => break Outcome::Timeout,
                    _ = cleanup_timer(cleanup_deadline), if cleanup_deadline.is_some() => {
                        report_incomplete(&mut sender, "normal collection", &io::Error::new(io::ErrorKind::TimedOut, "cleanup deadline elapsed"));
                        break Outcome::Collected(Err(io::Error::other("normal collection pending at cleanup deadline")));
                    }
                }
            };
            // Stamp at the observed trigger, before destruction of unfinished
            // inline work. A synchronous Drop cannot earn a new cleanup budget.
            cleanup_deadline.get_or_insert_with(|| Instant::now() + CLEANUP_BUDGET);
            // Dropping inline work closes every outstanding pipe. There are no
            // reader/writer JoinHandles that could detach here.
            outcome
        }
    };
    let deadline =
        cleanup_deadline.unwrap_or_else(|| tokio::time::sleep(CLEANUP_BUDGET).deadline());
    if !signalled {
        signal_error = signal_owned_group(group).err();
    }
    let cause = outcome.cause();
    if let Some(error) = signal_error {
        report_incomplete(&mut sender, &cause, &error);
    }
    let settlement = async {
        child.wait().await?;
        wait_group_absent(group).await
    };
    finish_settlement(settlement, deadline, outcome, sender, keepalive).await;
}

// One return budget spans every cleanup phase. Reporting an incomplete result
// does not transfer custody: the same task retains resources while settling.
async fn finish_settlement<K, S, T>(
    settlement: S,
    deadline: Instant,
    outcome: Outcome<T>,
    mut sender: Option<ResultSender<T>>,
    keepalive: K,
) where
    S: Future<Output = io::Result<()>>,
{
    let _keepalive = keepalive;
    let cause = outcome.cause();
    if Instant::now() >= deadline {
        report_incomplete(
            &mut sender,
            &cause,
            &io::Error::new(io::ErrorKind::TimedOut, "cleanup deadline elapsed"),
        );
    }
    tokio::pin!(settlement);
    match tokio::time::timeout_at(deadline, &mut settlement).await {
        Ok(Ok(())) => {}
        Ok(Err(error)) => {
            report_incomplete(&mut sender, &cause, &error);
            // A failed wait cannot certify reap. Do not release resources based
            // on a failed signal/wait or a possibly recycled numeric identity.
            std::future::pending::<()>().await;
        }
        Err(_) => {
            report_incomplete(
                &mut sender,
                &cause,
                &io::Error::new(io::ErrorKind::TimedOut, "cleanup deadline elapsed"),
            );
            if settlement.await.is_err() {
                std::future::pending::<()>().await;
            }
        }
    }
    if let Some(sender) = sender {
        let result = match outcome {
            Outcome::Collected(result) => result.map(Some),
            Outcome::Timeout | Outcome::Cancelled => Ok(None),
        };
        let _ = sender.send(result);
    }
}

async fn cleanup_timer(deadline: Option<Instant>) {
    match deadline {
        Some(deadline) => tokio::time::sleep_until(deadline).await,
        None => std::future::pending().await,
    }
}

async fn caller_closed<T>(sender: &mut Option<ResultSender<T>>) {
    match sender {
        Some(sender) => sender.closed().await,
        None => std::future::pending().await,
    }
}

fn report_incomplete<T>(sender: &mut Option<ResultSender<T>>, cause: &str, error: &io::Error) {
    if let Some(sender) = sender.take() {
        let _ = sender.send(Err(io::Error::other(format!(
            "subprocess cleanup incomplete ({cause}): {error}"
        ))));
    }
}

#[cfg(target_os = "linux")]
type ExitSignals = tokio::signal::unix::Signal;

#[cfg(target_os = "linux")]
fn exit_signals() -> io::Result<ExitSignals> {
    tokio::signal::unix::signal(tokio::signal::unix::SignalKind::child())
}

#[cfg(not(target_os = "linux"))]
type ExitSignals = ();

#[cfg(not(target_os = "linux"))]
fn exit_signals() -> io::Result<ExitSignals> {
    Err(io::Error::new(io::ErrorKind::Unsupported, "process custody currently requires Linux"))
}

#[cfg(not(target_os = "linux"))]
async fn observe_exit(_group: Option<i32>, _signals: &mut ExitSignals) -> io::Result<ExitStatus> {
    Err(io::Error::new(io::ErrorKind::Unsupported, "non-reaping observation requires Linux"))
}

#[cfg(not(target_os = "linux"))]
fn signal_owned_group(_group: Option<i32>) -> io::Result<()> {
    Err(io::Error::new(io::ErrorKind::Unsupported, "owned group validation requires Linux"))
}

#[cfg(target_os = "linux")]
fn exit_without_reaping(group: Option<i32>) -> io::Result<Option<ExitStatus>> {
    use std::os::unix::process::ExitStatusExt;
    let pid = group
        .filter(|pid| *pid > 0)
        .ok_or_else(|| io::Error::other("missing owned child identity"))?;
    loop {
        let mut information = std::mem::MaybeUninit::<libc::siginfo_t>::zeroed();
        // SAFETY: information points to writable siginfo_t storage; P_PID and
        // WNOWAIT inspect only the retained direct child and do not consume it.
        let code = unsafe {
            libc::waitid(
                libc::P_PID,
                pid as libc::id_t,
                information.as_mut_ptr(),
                libc::WEXITED | libc::WNOWAIT | libc::WNOHANG,
            )
        };
        if code != 0 {
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(error);
        }
        // SAFETY: successful waitid initialized the zeroed siginfo_t, including
        // the Linux SIGCHLD union members accessed by si_pid/si_status.
        let information = unsafe { information.assume_init() };
        let (reported, status) = unsafe { (information.si_pid(), information.si_status()) };
        if reported == 0 {
            return Ok(None);
        }
        let raw = match information.si_code {
            libc::CLD_EXITED => status << 8,
            libc::CLD_KILLED => status,
            libc::CLD_DUMPED => status | 0x80,
            _ => return Err(io::Error::other("unexpected owned-child wait status")),
        };
        return Ok(Some(ExitStatus::from_raw(raw)));
    }
}

#[cfg(target_os = "linux")]
async fn observe_exit(group: Option<i32>, signals: &mut ExitSignals) -> io::Result<ExitStatus> {
    loop {
        if let Some(status) = exit_without_reaping(group)? {
            return Ok(status);
        }
        if signals.recv().await.is_none() {
            return Err(io::Error::other("child exit notification closed"));
        }
    }
}

#[cfg(target_os = "linux")]
fn signal_owned_group(group: Option<i32>) -> io::Result<()> {
    let pid = group
        .filter(|pid| *pid > 0)
        .ok_or_else(|| io::Error::other("missing owned group identity"))?;
    // Validate direct-child custody before the LAST destructive signal. Once
    // Child::wait consumes status, no code path may signal this numeric group.
    exit_without_reaping(Some(pid))?;
    // SAFETY: getpgrp/getpgid have no memory preconditions; pid is the retained
    // child. Reject a moved group or the runtime's group before negative-PID kill.
    if unsafe { libc::getpgrp() == pid || libc::getpgid(pid) != pid } {
        return Err(io::Error::other("owned child no longer identifies its private group"));
    }
    // SAFETY: the positive group was validated while its direct leader remains
    // unreaped, so it cannot be a recycled group belonging to another execution.
    if unsafe { libc::kill(-pid, libc::SIGKILL) } == 0 {
        return Ok(());
    }
    let error = io::Error::last_os_error();
    if error.raw_os_error() == Some(libc::ESRCH) { Ok(()) } else { Err(error) }
}

#[cfg(unix)]
async fn wait_group_absent(group: Option<i32>) -> io::Result<()> {
    let pid = group
        .filter(|pid| *pid > 0)
        .ok_or_else(|| io::Error::other("missing owned group identity"))?;
    loop {
        // SAFETY: signal zero is non-destructive. Group reuse after reap can
        // conservatively retain resources, but never authorizes another kill.
        let present = unsafe { libc::kill(-pid, 0) };
        if present != 0 {
            let error = io::Error::last_os_error();
            if error.raw_os_error() == Some(libc::ESRCH) {
                return Ok(());
            }
            return Err(error);
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

#[cfg(not(unix))]
async fn wait_group_absent(_group: Option<i32>) -> io::Result<()> {
    Err(io::Error::new(io::ErrorKind::Unsupported, "process groups require Unix"))
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::os::fd::AsRawFd;
    use std::os::unix::net::UnixStream;
    use std::process::Stdio;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    struct DropNotice(Option<oneshot::Sender<()>>);

    impl Drop for DropNotice {
        fn drop(&mut self) {
            if let Some(notice) = self.0.take() {
                let _ = notice.send(());
            }
        }
    }

    struct DelayedCollectionDrop {
        _pipes: ProcessIo,
        ready: UnixStream,
        release: UnixStream,
    }

    impl Future for DelayedCollectionDrop {
        type Output = io::Result<()>;

        fn poll(
            self: std::pin::Pin<&mut Self>,
            _: &mut std::task::Context<'_>,
        ) -> Poll<Self::Output> {
            Poll::Ready(Err(io::Error::other("controlled collection failure")))
        }
    }

    impl Drop for DelayedCollectionDrop {
        fn drop(&mut self) {
            self.ready.write_all(b"D").expect("notify actual collection Drop");
            let mut byte = [0];
            self.release.read_exact(&mut byte).expect("release controlled collection Drop");
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn collection_drop_time_does_not_grant_a_fresh_cleanup_budget() {
        let (mut ready, notify) = UnixStream::pair().expect("Drop readiness pipe");
        let (release, mut released) = UnixStream::pair().expect("Drop release pipe");
        ready.set_read_timeout(Some(Duration::from_secs(5))).expect("bounded readiness");
        release.set_read_timeout(Some(Duration::from_secs(5))).expect("bounded Drop release");
        let mut command = Command::new("/usr/bin/python3");
        command
            .args(["-c", "import sys; sys.stdin.buffer.read()"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .process_group(0)
            .kill_on_drop(true);
        let signals = exit_signals().expect("register before spawn");
        let child = command.spawn().expect("real owned child");
        tokio::time::pause();
        let runtime = tokio::runtime::Handle::current();
        let controller = std::thread::spawn(move || {
            let mut byte = [0];
            ready.read_exact(&mut byte).expect("collector is being destroyed");
            runtime.block_on(tokio::time::advance(Duration::from_secs(6)));
            released.write_all(b"R").expect("release actual Drop");
        });
        let (result, receiver) = oneshot::channel();
        let (dropped, drop_notice) = oneshot::channel();
        let owner = tokio::spawn(own_child(
            StartedCommand {
                child,
                deadline: Instant::now() + Duration::from_secs(60),
                signals,
                keepalive: DropNotice(Some(dropped)),
            },
            CompletionBoundary::OutputEof,
            move |pipes| DelayedCollectionDrop { _pipes: pipes, ready: notify, release },
            result,
        ));
        let error =
            receiver.await.expect("owner returns incomplete status").expect_err("expired budget");
        tokio::time::resume();
        assert!(
            error
                .to_string()
                .contains("cleanup incomplete (collection error: controlled collection failure)")
        );
        controller.join().expect("actual Drop controller join");
        tokio::time::timeout(Duration::from_secs(5), owner)
            .await
            .expect("real child settlement watchdog")
            .expect("actual physical owner join");
        drop_notice.await.expect("guard released after actual settlement");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn cleanup_budget_spans_phases_and_retains_resources_after_incomplete_return() {
        tokio::time::pause();
        let deadline = Instant::now() + CLEANUP_BUDGET;
        let (started, start) = oneshot::channel();
        let (next_phase, next) = oneshot::channel();
        let (second_started, second_start) = oneshot::channel();
        let (settled, settle) = oneshot::channel();
        let (dropped, mut drop_notice) = oneshot::channel();
        let (result, mut receiver) = oneshot::channel::<io::Result<Option<()>>>();
        let settlement = async move {
            let _ = started.send(());
            next.await.map_err(io::Error::other)?;
            let _ = second_started.send(());
            settle.await.map_err(io::Error::other)?;
            Ok(())
        };
        let owner = tokio::spawn(finish_settlement(
            settlement,
            deadline,
            Outcome::Timeout,
            Some(result),
            DropNotice(Some(dropped)),
        ));
        start.await.expect("first cleanup phase actually polled");
        tokio::time::advance(Duration::from_secs(3)).await;
        assert!(matches!(receiver.try_recv(), Err(oneshot::error::TryRecvError::Empty)));
        next_phase.send(()).expect("advance cleanup phase");
        second_start.await.expect("second cleanup phase actually polled");
        tokio::time::advance(Duration::from_secs(3)).await;
        tokio::time::resume();
        let error =
            receiver.await.expect("bounded cleanup return").expect_err("incomplete cleanup");
        assert!(error.to_string().contains("cleanup incomplete (work timeout)"));
        assert!(matches!(drop_notice.try_recv(), Err(oneshot::error::TryRecvError::Empty)));
        settled.send(()).expect("owner still holds settlement future");
        owner.await.expect("actual owner join");
        drop_notice.await.expect("release follows actual settlement");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn delayed_spawn_preserves_complete_post_spawn_work_allowance() {
        let (mut ready_read, ready_write) = UnixStream::pair().expect("launch ready pipe");
        let (release_read, mut release_write) = UnixStream::pair().expect("launch release pipe");
        let mut command = Command::new("/usr/bin/python3");
        command
            .args(["-c", "import sys; sys.stdin.buffer.read(1); print('complete')"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        // SAFETY: this pre-exec closure calls only read/write with pre-existing
        // descriptors; no allocator, lock or non-signal-safe code runs after fork.
        unsafe {
            command.pre_exec(move || {
                let mut byte = [0u8];
                if libc::write(ready_write.as_raw_fd(), b"R".as_ptr().cast(), 1) != 1 {
                    return Err(io::Error::last_os_error());
                }
                if libc::read(release_read.as_raw_fd(), byte.as_mut_ptr().cast(), 1) != 1 {
                    return Err(io::Error::last_os_error());
                }
                Ok(())
            });
        }
        tokio::time::pause();
        let runtime = tokio::runtime::Handle::current();
        let controller = std::thread::spawn(move || {
            let mut ready = [0u8];
            ready_read.read_exact(&mut ready).expect("real launch reached barrier");
            assert_eq!(&ready, b"R");
            runtime.block_on(tokio::time::advance(Duration::from_secs(40)));
            release_write.write_all(b"X").expect("release real spawn");
        });
        let (started, started_receiver) = oneshot::channel();
        let (release_work, released) = oneshot::channel();
        let owner = tokio::spawn(run_owned_command(
            command,
            Duration::from_secs(60),
            (),
            CompletionBoundary::OutputEof,
            move |mut pipes| async move {
                let _ = started.send(());
                released.await.map_err(io::Error::other)?;
                if let Some(mut stdin) = pipes.stdin.take() {
                    stdin.write_all(b"X").await?;
                }
                let stdout = async {
                    let mut bytes = Vec::new();
                    if let Some(mut stdout) = pipes.stdout.take() {
                        stdout.read_to_end(&mut bytes).await?;
                    }
                    Ok::<_, io::Error>(bytes)
                };
                let status = async { pipes.status.await.map_err(io::Error::other)? };
                let (status, stdout) = tokio::try_join!(status, stdout)?;
                Ok((status, stdout))
            },
        ));
        started_receiver.await.expect("successful spawn");
        tokio::time::advance(Duration::from_secs(30)).await;
        tokio::time::resume();
        release_work.send(()).expect("work barrier still pending");
        controller.join().expect("launch controller actual join");
        let (status, stdout) = tokio::time::timeout(Duration::from_secs(10), owner)
            .await
            .expect("owner watchdog")
            .expect("owner task")
            .expect("owner result")
            .expect("S+W>T, W<T must retain normal work result");
        assert!(status.success());
        assert_eq!(stdout, b"complete\n");
    }
}
