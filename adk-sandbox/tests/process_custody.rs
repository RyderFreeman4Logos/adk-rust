//! Public cancellation and ordinary-completion controls with real owned processes.
#![cfg(target_os = "linux")]

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};

const PYTHON: &str = "/usr/bin/python3";
const WATCHDOG: Duration = Duration::from_secs(20);

// Only this dedicated fixture supervisor is a subreaper. It owns the re-executed
// test and actually waits for every orphan adopted after the SDK leader exits.
const SUPERVISOR: &str = r#"
import ctypes, json, os, pathlib, signal, subprocess, sys, time
root, binary, name = sys.argv[1:]
root = pathlib.Path(root)
libc = ctypes.CDLL(None, use_errno=True)
if libc.prctl(36, 1, 0, 0, 0) != 0:
    raise OSError(ctypes.get_errno(), 'fixture subreaper')
# Preserve the original 45-second total bound; reserve its last five seconds
# for failure cleanup instead of raising out before actual wait.
deadline = time.monotonic() + 45
failures = []
reaped = []
code = None
all_waited = False
child = None
def watchdog(signum, frame):
    raise TimeoutError('HARNESS_WATCHDOG, never semantic RED')
def kill_instance(pid, expected_start=None, direct=False):
    fd = os.pidfd_open(pid)
    try:
        if direct:
            # The dedicated supervisor is the sole reaper. Successful WNOWAIT
            # proves current direct-child custody without consuming its identity.
            os.waitid(os.P_PID, pid, os.WEXITED | os.WNOWAIT | os.WNOHANG)
        else:
            raw = pathlib.Path('/proc/%s/stat' % pid).read_text()
            if int(raw[raw.rfind(')') + 2:].split()[19]) != expected_start:
                return
        signal.pidfd_send_signal(fd, signal.SIGKILL)
    finally:
        os.close(fd)
def kill_recorded():
    try:
        lines = (root / 'identities.jsonl').read_text().splitlines()
    except FileNotFoundError:
        return
    except Exception as error:
        failures.append('identity file: %r' % error)
        return
    for line in lines:
        try:
            identity = json.loads(line)
            kill_instance(identity['pid'], identity['start'])
        except (ProcessLookupError, FileNotFoundError):
            pass
        except Exception as error:
            failures.append('identity row: %r' % error)
def kill_direct_children():
    # Exact kernel-owned children of this dedicated subreaper, including a
    # broker or newly adopted child not present in the fixture's identity log.
    children = pathlib.Path('/proc/self/task/%s/children' % os.getpid()).read_text().split()
    for value in children:
        try:
            kill_instance(int(value), direct=True)
        except (ChildProcessError, ProcessLookupError, FileNotFoundError):
            pass
        except Exception as error:
            failures.append('direct child: %r' % error)
def record_wait(pid, status):
    global code
    if child is not None and pid == child.pid:
        code = os.waitstatus_to_exitcode(status)
        child.returncode = code
    else:
        reaped.append({'pid': pid, 'status': status})
signal.signal(signal.SIGALRM, watchdog)
signal.alarm(40)
try:
    env = dict(os.environ, ADK_CUSTODY_INNER=str(root))
    control = name.startswith('watchdog_control')
    if control:
        ready_read, ready_write = os.pipe()
        env['ADK_CUSTODY_SUPERVISOR_READY_FD'] = str(ready_read)
    child = subprocess.Popen([binary, '--exact', name, '--nocapture'], env=env,
                             pass_fds=(ready_read,) if control else ())
    if control:
        os.close(ready_read)
        os.write(ready_write, b'R')
        os.close(ready_write)
    while True:
        try:
            pid, status = os.waitpid(-1, 0)
            record_wait(pid, status)
        except ChildProcessError:
            all_waited = True
            break
        except InterruptedError:
            continue
except BaseException as error:
    failures.append('%s: %s' % (type(error).__name__, error))
finally:
    signal.alarm(0)
    cleanup_deadline = min(deadline, time.monotonic() + 5)
    if not all_waited:
        # Identity errors never skip the finalizer or the remaining identities.
        kill_recorded()
        while True:
            try:
                kill_direct_children()
            except Exception as error:
                failures.append('direct-child census: %r' % error)
            try:
                while True:
                    pid, status = os.waitpid(-1, os.WNOHANG)
                    if pid == 0:
                        break
                    record_wait(pid, status)
            except ChildProcessError:
                all_waited = True
                break
            except InterruptedError:
                continue
            except Exception as error:
                failures.append('actual wait: %r' % error)
            remaining = cleanup_deadline - time.monotonic()
            if remaining <= 0:
                failures.append('bounded finalizer incomplete')
                break
            time.sleep(min(0.01, remaining))
    print('SUPERVISOR_REAP ' + json.dumps({'inner_pid': child.pid if child else None,
          'inner_exit': code, 'adopted': reaped, 'all_waited': all_waited,
          'harness_failure': bool(failures), 'failures': failures}), flush=True)
if failures or not all_waited or code is None:
    sys.exit(124)
sys.exit(code if code >= 0 else 128 - code)
"#;

// This is emergency cleanup, not the correctness oracle. Open pidfds FIRST,
// validate the recorded start time, then signal only that exact process instance.
const CLEANUP: &str = r#"
import errno, json, os, pathlib, select, signal, sys
records = pathlib.Path(sys.argv[1])
identities = [json.loads(line) for line in records.read_text().splitlines()] if records.exists() else []
ended = []
for record in identities:
    pid = record['pid']
    try:
        fd = os.pidfd_open(pid)
    except OSError as error:
        try:
            raw = pathlib.Path('/proc/%s/stat' % pid).read_text()
        except FileNotFoundError:
            ended.append({'pid': pid, 'already_gone': True})
            continue
        fields = raw[raw.rfind(')') + 2:].split()
        if int(fields[19]) != record['start'] or fields[0] in ('Z', 'X'):
            ended.append({'pid': pid, 'original_not_live': True, 'state': fields[0], 'pidfd_open_errno': error.errno})
            continue
        raise
    try:
        try:
            raw = pathlib.Path('/proc/%s/stat' % pid).read_text()
        except FileNotFoundError:
            ended.append(pid)
            continue
        start = int(raw[raw.rfind(')') + 2:].split()[19])
        if start != record['start']:
            continue
        try:
            signal.pidfd_send_signal(fd, signal.SIGKILL)
        except ProcessLookupError:
            pass
        poll = select.poll()
        poll.register(fd, select.POLLIN)
        if not poll.poll(5000):
            raise RuntimeError('fixture emergency cleanup did not observe death')
        ended.append(pid)
    finally:
        os.close(fd)
print('FIXTURE_CLEANUP_DEATH ' + json.dumps(ended), flush=True)
"#;

// The broker owns an actual accept/read handshake. Its stdout is the test's
// readiness channel; the SDK's own stdout/stderr remain independently controlled.
const BROKER: &str = r#"
import json, os, pathlib, select, socket, sys
listener = socket.socket(socket.AF_UNIX)
listener.bind(sys.argv[1])
listener.listen(1)
listener.settimeout(15)
print(json.dumps({'listening': True}), flush=True)
conn, _ = listener.accept()
conn.settimeout(15)
stream = conn.makefile('rb')
ready = json.loads(stream.readline())
leader = ready['leader']
leader_fd = os.pidfd_open(leader['pid'])
raw = pathlib.Path('/proc/%s/stat' % leader['pid']).read_text()
assert int(raw[raw.rfind(')') + 2:].split()[19]) == leader['start']
print(json.dumps(ready), flush=True)
for command in sys.stdin:
    command = command.strip()
    if command == 'Q':
        break
    conn.sendall(command.encode())
    if command == 'E':
        events = select.poll()
        events.register(leader_fd, select.POLLIN)
        if not events.poll(5000):
            raise RuntimeError('broker leader exit watchdog')
        print(json.dumps({'leader_exit_requested': True, 'leader_exit_observed': True}), flush=True)
        continue
    response = stream.readline()
    if not response:
        print(json.dumps({'peer_closed': True}), flush=True)
    else:
        print(response.decode().strip(), flush=True)
conn.close()
os.close(leader_fd)
listener.close()
"#;

// The descendant remains in the SDK-owned group. Its explicit control socket
// holds it alive independently of inherited output pipes and stdin consumption.
const PROGRAM: &str = r#"#!/usr/bin/python3
import hashlib, json, os, pathlib, select, shutil, socket, sys
ROOT = __ROOT__
MODE = __MODE__
CLOSED = __CLOSED__
root = pathlib.Path(ROOT)
source = pathlib.Path(sys.argv[1]) if len(sys.argv) > 1 else pathlib.Path(__file__).parent / 'main.rs'
output = pathlib.Path(sys.argv[sys.argv.index('-o') + 1]) if '-o' in sys.argv else pathlib.Path(__file__)
if MODE == 'install':
    shutil.copyfile(root / 'runtime', output)
    output.chmod(0o700)
    sys.exit(0)
def identity(pid):
    raw = pathlib.Path('/proc/%s/stat' % pid).read_text()
    return {'pid': pid, 'start': int(raw[raw.rfind(')') + 2:].split()[19])}
def record(data):
    fd = os.open(root / 'identities.jsonl', os.O_WRONLY | os.O_CREAT | os.O_APPEND, 0o600)
    os.write(fd, (json.dumps(data) + '\n').encode())
    os.close(fd)
leader = identity(os.getpid())
record(leader)
read_end, write_end = os.pipe()
pid = os.fork()
if pid:
    os.close(write_end)
    if os.read(read_end, 1) != b'R':
        raise RuntimeError('descendant readiness failed')
    if MODE in ('normal', 'stdin'):
        if os.read(read_end, 1) != b'X':
            raise RuntimeError('leader release handshake failed')
        os.close(read_end)
        os._exit(1 if MODE == 'normal' else 0)
    os.close(read_end)
    os.waitpid(pid, 0)
    os._exit(1)
os.close(read_end)
if CLOSED or MODE == 'stdin':
    for fd in (1, 2):
        os.close(fd)
    if MODE != 'stdin':
        os.close(0)
conn = socket.socket(socket.AF_UNIX)
conn.connect(str(root / 'control.sock'))
me = identity(os.getpid())
record(me)
leader_fd = os.pidfd_open(leader['pid'])
ready = dict(me, leader=leader, pgid=os.getpgrp(), source=str(source), output=str(output), source_sha=hashlib.sha256(source.read_bytes()).hexdigest(), mode=MODE, closed=CLOSED)
conn.sendall((json.dumps(ready) + '\n').encode())
os.write(write_end, b'R')
if MODE not in ('normal', 'stdin'):
    os.close(write_end)
while True:
    command = conn.recv(1)
    if not command:
        break
    if command == b'E':
        os.write(write_end, b'X')
        os.close(write_end)
        leader_events = select.poll()
        leader_events.register(leader_fd, select.POLLIN)
        if not leader_events.poll(5000):
            raise RuntimeError('leader exit handshake watchdog')
    if command in (b'P', b'R'):
        exists = source.exists()
        reply = {'source_exists': exists, 'source_sha': hashlib.sha256(source.read_bytes()).hexdigest() if exists else None}
        conn.sendall((json.dumps(reply) + '\n').encode())
        if command == b'R':
            break
conn.close()
os.close(leader_fd)
os._exit(0)
"#;

// Death can precede actual wait/group absence and artifact destruction. Observe
// disappearance using the remainder of the SAME absolute correctness watchdog.
async fn wait_for_artifact_release(source: &Path, deadline: tokio::time::Instant) -> bool {
    let released = loop {
        if tokio::time::Instant::now() >= deadline {
            break false;
        }
        if !source.exists() {
            break true;
        }
        tokio::time::sleep_until(
            deadline.min(tokio::time::Instant::now() + Duration::from_millis(10)),
        )
        .await;
    };
    eprintln!("ARTIFACT_RELEASE_OBSERVED path={} released={released}", source.display());
    released
}

#[tokio::test(flavor = "current_thread")]
async fn artifact_observer_waits_when_death_precedes_owner_release() {
    let directory = tempfile::tempdir().expect("artifact control directory");
    let source = directory.path().join("source");
    std::fs::write(&source, b"still owned after child death").expect("artifact source");
    let output = Command::new("/bin/true").output().await.expect("real control child actual wait");
    assert!(output.status.success());
    let (release, released) = tokio::sync::oneshot::channel();
    let owner = tokio::spawn(async move {
        released.await.expect("controlled owner release");
        drop(directory);
    });
    let deadline = tokio::time::Instant::now() + WATCHDOG;
    let mut observation = Box::pin(wait_for_artifact_release(&source, deadline));
    std::future::poll_fn(|cx| {
        assert!(std::future::Future::poll(observation.as_mut(), cx).is_pending());
        std::task::Poll::Ready(())
    })
    .await;
    assert!(source.exists(), "death alone cannot release the owner artifact");
    release.send(()).expect("release owner after death observation");
    assert!(observation.await, "observe actual subsequent deletion");
    owner.await.expect("actual controlled owner join");
}

#[tokio::test(flavor = "current_thread")]
async fn artifact_observer_rejects_retention_at_the_original_deadline() {
    let directory = tempfile::tempdir().expect("retained artifact control");
    let source = directory.path().join("source");
    std::fs::write(&source, b"retained").expect("retained source");
    tokio::time::pause();
    let deadline = tokio::time::Instant::now() + WATCHDOG;
    let mut observation = Box::pin(wait_for_artifact_release(&source, deadline));
    std::future::poll_fn(|cx| {
        assert!(std::future::Future::poll(observation.as_mut(), cx).is_pending());
        std::task::Poll::Ready(())
    })
    .await;
    tokio::time::advance(WATCHDOG + Duration::from_secs(1)).await;
    tokio::time::resume();
    assert!(!observation.await, "retention must fail at the unchanged absolute deadline");
    assert!(source.exists(), "observer must never manufacture resource cleanup");
}

// Force the ACTUAL supervisor watchdog after a pipe barrier, with the inner
// and an already adopted descendant alive. A malformed identity row additionally
// proves that identity diagnostics cannot skip the finalizer or actual waits.
const WATCHDOG_INNER: &str = r#"#!/usr/bin/python3
import json, os, pathlib, signal, sys
root = pathlib.Path(os.environ['ADK_CUSTODY_INNER'])
ready = int(os.environ['ADK_CUSTODY_SUPERVISOR_READY_FD'])
assert os.read(ready, 1) == b'R'
os.close(ready)
def identity():
    pid = os.getpid()
    raw = pathlib.Path('/proc/%s/stat' % pid).read_text()
    return {'pid': pid, 'start': int(raw[raw.rfind(')') + 2:].split()[19])}
def record(item):
    fd = os.open(root / 'identities.jsonl', os.O_WRONLY | os.O_CREAT | os.O_APPEND, 0o600)
    os.write(fd, (json.dumps(item) + '\n').encode())
    os.close(fd)
me = identity()
record(me)
if 'identity_error' in sys.argv:
    with (root / 'identities.jsonl').open('a') as log:
        log.write('invalid-identity-row\n')
read_end, write_end = os.pipe()
bridge = os.fork()
if bridge == 0:
    descendant = os.fork()
    if descendant:
        os._exit(0)
    os.close(read_end)
    record(identity())
    os.write(write_end, b'D')
    os.close(write_end)
    while True:
        signal.pause()
os.close(write_end)
assert os.read(read_end, 1) == b'D'
os.close(read_end)
os.waitpid(bridge, 0)
records = []
for line in (root / 'identities.jsonl').read_text().splitlines():
    try:
        records.append(json.loads(line))
    except json.JSONDecodeError:
        pass
descendant = next(item for item in records if item['pid'] != me['pid'])
raw = pathlib.Path('/proc/%s/stat' % descendant['pid']).read_text()
fields = raw[raw.rfind(')') + 2:].split()
assert int(fields[19]) == descendant['start']
assert fields[0] not in ('Z', 'X')
assert int(fields[1]) == os.getppid(), 'descendant must already be adopted by supervisor'
print('HARNESS_CONTROL_READY ' + json.dumps({'inner': me, 'adopted': descendant, 'adopted_alive': True}), flush=True)
os.kill(os.getppid(), signal.SIGALRM)
while True:
    signal.pause()
"#;

#[tokio::test(flavor = "current_thread")]
async fn supervisor_watchdog_finally_reaps_live_inner_and_adopted_instances() {
    for identity_error in [false, true] {
        let directory = tempfile::tempdir().expect("watchdog control directory");
        let executable = directory.path().join("watchdog-inner");
        let script = if identity_error {
            WATCHDOG_INNER.replace("'identity_error' in sys.argv", "True")
        } else {
            WATCHDOG_INNER.to_string()
        };
        std::fs::write(&executable, script).expect("watchdog inner script");
        std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o700))
            .expect("watchdog inner executable");
        let output = Command::new(PYTHON)
            .args(["-c", SUPERVISOR])
            .arg(directory.path())
            .arg(&executable)
            .arg("watchdog_control")
            .output()
            .await
            .expect("actual watchdog supervisor output and wait");
        let stdout = String::from_utf8(output.stdout).expect("control stdout");
        eprintln!("{stdout}");
        eprintln!("{}", String::from_utf8_lossy(&output.stderr));
        assert_eq!(output.status.code(), Some(124), "must be harness failure, never semantic RED");
        assert!(stdout.contains("HARNESS_CONTROL_READY "), "watchdog follows actual readiness");
        let receipt: Value = serde_json::from_str(
            stdout
                .lines()
                .find_map(|line| line.strip_prefix("SUPERVISOR_REAP "))
                .expect("actual supervisor wait receipt"),
        )
        .expect("supervisor receipt JSON");
        assert_eq!(receipt["all_waited"], true);
        assert_eq!(receipt["harness_failure"], true);
        assert_ne!(receipt["inner_exit"], 0);
        assert!(!receipt["adopted"].as_array().expect("adopted waits").is_empty());
        let identities = std::fs::read_to_string(directory.path().join("identities.jsonl"))
            .expect("actual original identities");
        let mut checked = 0;
        for line in identities.lines() {
            let Ok(identity) = serde_json::from_str::<Value>(line) else { continue };
            let pid = identity["pid"].as_u64().expect("control PID");
            let same =
                std::fs::read_to_string(format!("/proc/{pid}/stat")).ok().is_some_and(|raw| {
                    raw.rsplit_once(") ")
                        .and_then(|(_, fields)| fields.split_whitespace().nth(19))
                        .and_then(|start| start.parse::<u64>().ok())
                        == identity["start"].as_u64()
                });
            eprintln!(
                "HARNESS_CONTROL_INSTANCE_ABSENCE identity={identity} original_present={same}"
            );
            assert!(!same, "original control instance must actually be absent after wait");
            checked += 1;
        }
        assert_eq!(checked, 2, "inner and adopted descendant original identities");
        if identity_error {
            assert!(receipt["failures"].as_array().expect("failure diagnostics").iter().any(
                |failure| failure.as_str().is_some_and(|message| message.contains("identity row"))
            ));
        }
    }
}

async fn supervised(name: &str) -> bool {
    if std::env::var_os("ADK_CUSTODY_INNER").is_some() {
        return false;
    }
    let tmp = std::env::var_os("TMPDIR").expect("Just must configure verified SSD TMPDIR");
    let directory =
        tempfile::Builder::new().prefix("adk-custody-").tempdir_in(tmp).expect("fixture directory");
    let output = Command::new(PYTHON)
        .args(["-c", SUPERVISOR])
        .arg(directory.path())
        .arg(std::env::current_exe().expect("current test binary"))
        .arg(name)
        .kill_on_drop(true)
        .output()
        .await
        .expect("fixture supervisor output");
    eprintln!("{}", String::from_utf8_lossy(&output.stdout));
    eprintln!("{}", String::from_utf8_lossy(&output.stderr));
    assert!(
        output.stdout.windows(b"\"all_waited\": true".len()).any(|v| v == b"\"all_waited\": true"),
        "fixture supervisor must actually wait all adopted processes"
    );
    assert!(
        output.status.success(),
        "public custody case failed, supervisor status {}",
        output.status
    );
    true
}

struct Fixture {
    root: PathBuf,
    executable: PathBuf,
    broker: Child,
    commands: ChildStdin,
    replies: BufReader<ChildStdout>,
    cleaned: bool,
}

impl Fixture {
    async fn new(mode: &str, closed: bool) -> Self {
        let root =
            PathBuf::from(std::env::var_os("ADK_CUSTODY_INNER").expect("supervised fixture"));
        let executable = root.join("compiler");
        let render = |mode: &str| {
            PROGRAM
                .replace("__ROOT__", &serde_json::to_string(&root).expect("root string"))
                .replace("__MODE__", &serde_json::to_string(mode).expect("mode string"))
                .replace("__CLOSED__", if closed { "True" } else { "False" })
        };
        std::fs::write(&executable, render(mode)).expect("fixture program");
        std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o700))
            .expect("executable mode");
        std::fs::write(root.join("runtime"), render("stdin")).expect("runtime fixture");
        let mut broker = Command::new(PYTHON)
            .args(["-c", BROKER])
            .arg(root.join("control.sock"))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .kill_on_drop(true)
            .spawn()
            .expect("control broker");
        let commands = broker.stdin.take().expect("broker stdin");
        let replies = BufReader::new(broker.stdout.take().expect("broker stdout"));
        let mut fixture = Self { root, executable, broker, commands, replies, cleaned: false };
        assert_eq!(fixture.message().await["listening"], true);
        fixture
    }

    async fn message(&mut self) -> Value {
        let mut line = String::new();
        let count = tokio::time::timeout(WATCHDOG, self.replies.read_line(&mut line))
            .await
            .expect("fixture handshake watchdog, never a semantic RED")
            .expect("fixture handshake read");
        assert_ne!(count, 0, "fixture handshake EOF, never a semantic RED");
        serde_json::from_str(&line).expect("fixture JSON handshake")
    }

    async fn command(&mut self, command: &[u8]) -> Value {
        self.commands.write_all(command).await.expect("fixture control command");
        self.message().await
    }

    fn observed(&self, ready: &Value) -> Value {
        let pid = ready["pid"].as_u64().expect("descendant PID");
        let raw = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok();
        let fields = raw.as_deref().and_then(|raw| {
            raw.rsplit_once(") ").map(|(_, tail)| tail.split_whitespace().collect::<Vec<_>>())
        });
        let same = fields.as_ref().is_some_and(|v| {
            v.get(19).and_then(|v| v.parse::<u64>().ok()) == ready["start"].as_u64()
        });
        let alive =
            same && fields.as_ref().and_then(|v| v.first()).is_some_and(|v| *v != "Z" && *v != "X");
        let source = Path::new(ready["source"].as_str().expect("source path"));
        let observed = json!({"ready": ready, "same_instance": same, "alive": alive, "source_exists": source.exists(), "output_directory_exists": Path::new(ready["output"].as_str().expect("output path")).parent().is_some_and(Path::exists)});
        eprintln!("PUBLIC_CUSTODY_OBSERVED {observed}");
        std::fs::write(self.root.join("observed.json"), observed.to_string())
            .expect("capture before emergency cleanup");
        observed
    }

    async fn natural_settlement(&self, ready: &Value, deadline: tokio::time::Instant) -> bool {
        let observation = Command::new(PYTHON)
            .args([
                "-c",
                r#"
import json, os, pathlib, select, sys
record = json.loads(sys.argv[1])
try:
    fd = os.pidfd_open(record['pid'])
except ProcessLookupError:
    sys.exit(0)
try:
    raw = pathlib.Path('/proc/%s/stat' % record['pid']).read_text()
    fields = raw[raw.rfind(')') + 2:].split()
    if int(fields[19]) != record['start'] or fields[0] in ('Z', 'X'):
        sys.exit(0)
    events = select.poll()
    events.register(fd, select.POLLIN)
    sys.exit(0 if events.poll(int(sys.argv[2])) else 1)
except FileNotFoundError:
    sys.exit(0)
finally:
    os.close(fd)
"#,
            ])
            .arg(ready.to_string())
            .arg(
                deadline
                    .saturating_duration_since(tokio::time::Instant::now())
                    .as_millis()
                    .min(10_000)
                    .to_string(),
            )
            .output()
            .await
            .expect("read-only natural settlement observer");
        observation.status.success()
    }

    async fn cleanup(&mut self) {
        let cleanup = Command::new(PYTHON)
            .args(["-c", CLEANUP])
            .arg(self.root.join("identities.jsonl"))
            .output()
            .await
            .expect("identity-scoped fixture cleanup");
        eprintln!("{}", String::from_utf8_lossy(&cleanup.stdout));
        eprintln!("{}", String::from_utf8_lossy(&cleanup.stderr));
        self.commands.write_all(b"Q\n").await.expect("broker shutdown");
        self.commands.shutdown().await.expect("broker stdin close");
        let mut tail = Vec::new();
        self.replies.read_to_end(&mut tail).await.expect("broker actual pipe drain");
        let status = self.broker.wait().await.expect("broker actual wait");
        self.cleaned = true;
        assert!(cleanup.status.success(), "fixture cleanup failed");
        assert!(status.success(), "broker actual wait failed: {status}");
        eprintln!("FIXTURE_BROKER_WAIT_DRAIN complete");
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        if !self.cleaned {
            let result = std::process::Command::new(PYTHON)
                .args(["-c", CLEANUP])
                .arg(self.root.join("identities.jsonl"))
                .output();
            eprintln!("UNCONDITIONAL_FIXTURE_CLEANUP {result:?}");
            // The dedicated supervisor actually waits this direct child if an
            // assertion unwinds before async cleanup can await its handle.
            let _ = self.broker.start_kill();
        }
    }
}

use adk_sandbox::{ExecRequest, Language, ProcessBackend, ProcessConfig, SandboxBackend};
use std::collections::HashMap;

fn backend(fixture: &Fixture) -> ProcessBackend {
    ProcessBackend::new(ProcessConfig {
        rustc_path: fixture.executable.to_string_lossy().into_owned(),
        python_path: fixture.executable.to_string_lossy().into_owned(),
        node_path: fixture.executable.to_string_lossy().into_owned(),
        ..ProcessConfig::default()
    })
}

fn request() -> ExecRequest {
    ExecRequest {
        language: Language::Python,
        code: "print('custody fixture')".to_string(),
        stdin: None,
        timeout: Duration::from_secs(30),
        memory_limit_mb: None,
        env: HashMap::new(),
    }
}

async fn backend_cancel(language: Language, closed: bool) {
    let mut fixture = Fixture::new("cancel", closed).await;
    let backend = backend(&fixture);
    let mut request = request();
    request.language = language;
    let mut operation = Box::pin(backend.execute(request));
    let ready = tokio::select! {
        ready = fixture.message() => ready,
        result = &mut operation => panic!("public backend returned before readiness: {result:?}"),
    };
    assert_eq!(fixture.command(b"P\n").await["source_exists"], true);
    let deadline = tokio::time::Instant::now() + WATCHDOG;
    drop(operation);
    let observed = fixture.observed(&ready);
    let violation = observed["alive"] == true && observed["source_exists"] == false;
    let settled = violation || fixture.natural_settlement(&ready, deadline).await;
    let released = violation
        || wait_for_artifact_release(
            Path::new(ready["source"].as_str().expect("source path")),
            deadline,
        )
        .await;
    fixture.cleanup().await;
    assert!(
        settled && released,
        "public cancellation must naturally settle and release before emergency cleanup"
    );
    assert!(
        !violation,
        "SDK32_SEMANTIC_RED: live backend descendant lost its source directory after public future cancellation: {observed}"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn backend_cancel_inherited_stdio_keeps_artifacts() {
    if supervised("backend_cancel_inherited_stdio_keeps_artifacts").await {
        return;
    }
    backend_cancel(Language::Python, false).await;
}

#[tokio::test(flavor = "current_thread")]
async fn backend_cancel_closed_stdio_keeps_artifacts() {
    if supervised("backend_cancel_closed_stdio_keeps_artifacts").await {
        return;
    }
    backend_cancel(Language::Python, true).await;
}

macro_rules! language_cancellation_case {
    ($name:ident, $language:expr, $closed:expr) => {
        #[tokio::test(flavor = "current_thread")]
        async fn $name() {
            if supervised(stringify!($name)).await {
                return;
            }
            backend_cancel($language, $closed).await;
        }
    };
}

language_cancellation_case!(rust_compile_cancel_inherited_stdio, Language::Rust, false);
language_cancellation_case!(rust_compile_cancel_closed_stdio, Language::Rust, true);
language_cancellation_case!(javascript_cancel_inherited_stdio, Language::JavaScript, false);
language_cancellation_case!(javascript_cancel_closed_stdio, Language::JavaScript, true);
language_cancellation_case!(typescript_cancel_inherited_stdio, Language::TypeScript, false);
language_cancellation_case!(typescript_cancel_closed_stdio, Language::TypeScript, true);

async fn backend_timeout(closed: bool) {
    let mut fixture = Fixture::new("cancel", closed).await;
    let backend = backend(&fixture);
    let mut operation = Box::pin(backend.execute(request()));
    let ready = tokio::select! {
        ready = fixture.message() => ready,
        result = &mut operation => panic!("backend returned before readiness: {result:?}"),
    };
    tokio::time::pause();
    tokio::time::advance(Duration::from_secs(31)).await;
    // Real kernel settlement must run with real time, before the next await.
    tokio::time::resume();
    let result =
        tokio::time::timeout(WATCHDOG, &mut operation).await.expect("work timeout return watchdog");
    drop(operation);
    let observed = fixture.observed(&ready);
    fixture.cleanup().await;
    assert!(matches!(result, Err(adk_sandbox::SandboxError::Timeout { .. })));
    assert_eq!(observed["alive"], false, "timeout must settle before return");
    assert_eq!(observed["source_exists"], false, "settled timeout releases artifacts");
}

#[tokio::test(flavor = "current_thread")]
async fn backend_timeout_inherited_stdio_settles_before_return() {
    if !supervised("backend_timeout_inherited_stdio_settles_before_return").await {
        backend_timeout(false).await;
    }
}

#[tokio::test(flavor = "current_thread")]
async fn backend_timeout_closed_stdio_settles_before_return() {
    if !supervised("backend_timeout_closed_stdio_settles_before_return").await {
        backend_timeout(true).await;
    }
}

fn shell_argument(path: &Path) -> String {
    format!("'{}'", path.to_str().expect("fixture UTF-8 path").replace('\'', "'\\''"))
}

#[tokio::test(flavor = "current_thread")]
async fn command_backend_transfers_caller_owned_artifact_to_physical_owner() {
    if supervised("command_backend_transfers_caller_owned_artifact_to_physical_owner").await {
        return;
    }
    let mut fixture = Fixture::new("cancel", true).await;
    let directory =
        std::sync::Arc::new(tempfile::tempdir().expect("caller-owned artifact directory"));
    let source = directory.path().join("caller-source.txt");
    std::fs::write(&source, b"caller-owned artifact").expect("caller source");
    let backend: std::sync::Arc<dyn SandboxBackend> = std::sync::Arc::new(backend(&fixture));
    let mut request = request();
    request.language = Language::Command;
    request.code =
        format!("exec {} {}", shell_argument(&fixture.executable), shell_argument(&source));
    let mut operation = Box::pin(backend.execute_with_keepalive(request, directory));
    let ready = tokio::select! {
        ready = fixture.message() => ready,
        result = &mut operation => panic!("command returned before readiness: {result:?}"),
    };
    assert_eq!(fixture.command(b"P\n").await["source_exists"], true);
    let deadline = tokio::time::Instant::now() + WATCHDOG;
    drop(operation);
    let observed = fixture.observed(&ready);
    let violation = observed["alive"] == true && observed["source_exists"] == false;
    let settled = !violation && fixture.natural_settlement(&ready, deadline).await;
    let released = wait_for_artifact_release(&source, deadline).await;
    fixture.cleanup().await;
    assert!(!violation && settled && released, "caller artifact must share physical custody");
}

#[cfg(feature = "workspace")]
async fn local_session_lifetime(cancel: bool, timeout: bool) {
    use adk_sandbox::workspace::{LocalUnixSession, SandboxSession};
    let mut fixture = Fixture::new(if cancel || timeout { "cancel" } else { "normal" }, true).await;
    let source = fixture.root.join("workspace-source.txt");
    std::fs::write(&source, b"workspace path is not an owned TempDir").expect("workspace source");
    let session = LocalUnixSession::new(fixture.root.clone(), Duration::from_secs(30));
    let command =
        format!("exec {} {}", shell_argument(&fixture.executable), shell_argument(&source));
    let mut operation = Box::pin(session.exec_command(&command, None));
    let ready = tokio::select! {
        ready = fixture.message() => ready,
        result = &mut operation => panic!("session returned before readiness: {result:?}"),
    };
    if cancel {
        let deadline = tokio::time::Instant::now() + WATCHDOG;
        drop(operation);
        assert!(
            fixture.natural_settlement(&ready, deadline).await,
            "session cancellation naturally settles"
        );
    } else {
        if timeout {
            tokio::time::pause();
            tokio::time::advance(Duration::from_secs(31)).await;
            tokio::time::resume();
        } else {
            assert_eq!(fixture.command(b"E\n").await["leader_exit_observed"], true);
        }
        let result =
            tokio::time::timeout(WATCHDOG, &mut operation).await.expect("session result watchdog");
        if timeout {
            let output = result.expect("legacy workspace timeout returns ExecOutput");
            assert!(output.timed_out);
            assert_eq!(output.exit_code, -1);
            assert!(output.stdout.is_empty() && output.stderr.is_empty());
        } else {
            assert_eq!(result.expect("normal session output").exit_code, 1);
        }
        drop(operation);
    }
    let observed = fixture.observed(&ready);
    fixture.cleanup().await;
    assert_eq!(observed["alive"], false);
    assert!(source.exists(), "workspace PathBuf is not ownership of its directory");
}

#[cfg(feature = "workspace")]
macro_rules! session_lifetime_case {
    ($name:ident, $cancel:expr, $timeout:expr) => {
        #[tokio::test(flavor = "current_thread")]
        async fn $name() {
            if !supervised(stringify!($name)).await {
                local_session_lifetime($cancel, $timeout).await;
            }
        }
    };
}

#[cfg(feature = "workspace")]
session_lifetime_case!(local_session_external_cancel, true, false);
#[cfg(feature = "workspace")]
session_lifetime_case!(local_session_internal_timeout, false, true);
#[cfg(feature = "workspace")]
session_lifetime_case!(local_session_normal_leader_exit, false, false);

#[tokio::test(flavor = "current_thread")]
async fn backend_normal_leader_exit_ends_inherited_descendant() {
    if supervised("backend_normal_leader_exit_ends_inherited_descendant").await {
        return;
    }
    normal_leader_exit(false).await;
}

#[tokio::test(flavor = "current_thread")]
async fn backend_normal_leader_exit_ends_closed_stdio_descendant() {
    if supervised("backend_normal_leader_exit_ends_closed_stdio_descendant").await {
        return;
    }
    normal_leader_exit(true).await;
}

async fn normal_leader_exit(closed: bool) {
    let mut fixture = Fixture::new("normal", closed).await;
    let backend = backend(&fixture);
    let mut operation = Box::pin(backend.execute(request()));
    let ready = tokio::select! {
        ready = fixture.message() => ready,
        result = &mut operation => panic!("backend returned before readiness: {result:?}"),
    };
    assert_eq!(fixture.command(b"E\n").await["leader_exit_requested"], true);
    let result = tokio::time::timeout(WATCHDOG, &mut operation)
        .await
        .expect("normal backend watchdog")
        .expect("ordinary backend output");
    drop(operation);
    let observed = fixture.observed(&ready);
    fixture.cleanup().await;
    assert_eq!(result.exit_code, 1);
    assert_eq!(observed["alive"], false, "normal leader-exit cleanup control");
}
