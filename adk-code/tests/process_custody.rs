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
source = (root / 'cli-source') if MODE == 'cli' else (pathlib.Path(sys.argv[1]) if len(sys.argv) > 1 else pathlib.Path(__file__).parent / 'main.rs')
output = pathlib.Path(sys.argv[sys.argv.index('-o') + 1]) if '-o' in sys.argv else pathlib.Path(__file__)
if MODE == 'build' and '--emit=metadata' in sys.argv:
    output.write_bytes(b'fixture metadata')
    sys.exit(0)
if MODE in ('install', 'binary'):
    if '--emit=metadata' in sys.argv:
        output.write_bytes(b'fixture metadata')
        sys.exit(0)
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
        if MODE == 'normal':
            print(json.dumps({'message': 'fixture compiler failure', 'level': 'error', 'code': None}), file=sys.stderr, flush=True)
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
ready = dict(me, leader=leader, pgid=os.getpgrp(), source=str(source), output=str(output), source_sha=hashlib.sha256(source.read_bytes()).hexdigest(), argv=sys.argv, metadata='--emit=metadata' in sys.argv, mode=MODE, closed=CLOSED)
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
        std::fs::write(
            root.join("runtime"),
            render(if mode == "binary" { "cancel" } else { "stdin" }),
        )
        .expect("runtime fixture");
        std::fs::write(root.join("cli-source"), b"fixture CLI resource").expect("CLI marker");
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

use adk_code::{
    CodeExecutor, ExecutionLanguage, ExecutionPayload, ExecutionRequest, ExecutionStatus,
    RustSandboxConfig, RustSandboxExecutor, SandboxPolicy,
};

fn request(timeout: Duration) -> ExecutionRequest {
    ExecutionRequest {
        language: ExecutionLanguage::Rust,
        payload: ExecutionPayload::Source {
            code: "fn run(v: serde_json::Value) -> serde_json::Value { v }".to_string(),
        },
        argv: Vec::new(),
        stdin: None,
        input: None,
        sandbox: SandboxPolicy { timeout, ..SandboxPolicy::host_local() },
        identity: None,
    }
}

fn executor(fixture: &Fixture) -> RustSandboxExecutor {
    let dependency = fixture.root.join("libserde_json.rlib");
    std::fs::write(&dependency, b"fixture-only absolute dependency path")
        .expect("dependency marker");
    RustSandboxExecutor::new(RustSandboxConfig {
        rustc_path: fixture.executable.to_string_lossy().into_owned(),
        rustc_flags: Vec::new(),
        serde_json_path: Some(dependency),
    })
}

async fn compile_cancel(closed: bool) {
    let mut fixture = Fixture::new("cancel", closed).await;
    let executor = executor(&fixture);
    let mut operation = Box::pin(executor.execute(request(Duration::from_secs(30))));
    let ready = tokio::select! {
        ready = fixture.message() => ready,
        result = &mut operation => panic!("public compile returned before readiness: {result:?}"),
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
        "SDK31_SEMANTIC_RED: live compiler descendant lost its source directory after public future cancellation: {observed}"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn host_compile_cancel_inherited_stdio_keeps_artifacts() {
    if supervised("host_compile_cancel_inherited_stdio_keeps_artifacts").await {
        return;
    }
    compile_cancel(false).await;
}

#[tokio::test(flavor = "current_thread")]
async fn host_compile_cancel_closed_stdio_keeps_artifacts() {
    if supervised("host_compile_cancel_closed_stdio_keeps_artifacts").await {
        return;
    }
    compile_cancel(true).await;
}

#[tokio::test(flavor = "current_thread")]
async fn host_normal_output_waits_for_descendant_source_use() {
    if supervised("host_normal_output_waits_for_descendant_source_use").await {
        return;
    }
    let mut fixture = Fixture::new("normal", false).await;
    let executor = executor(&fixture);
    let mut operation = Box::pin(executor.execute(request(Duration::from_secs(30))));
    let ready = tokio::select! {
        ready = fixture.message() => ready,
        result = &mut operation => panic!("public output returned before readiness: {result:?}"),
    };
    assert_eq!(fixture.command(b"E\n").await["leader_exit_requested"], true);
    let observed = fixture.observed(&ready);
    let ack = tokio::select! {
        ack = fixture.command(b"P\n") => ack,
        result = &mut operation => panic!("normal Output completed before descendant EOF: {result:?}"),
    };
    assert_eq!(ack["source_sha"], ready["source_sha"]);
    assert_eq!(observed["source_exists"], true);
    let release = fixture.command(b"R\n").await;
    assert_eq!(release["source_exists"], true);
    let result = tokio::time::timeout(WATCHDOG, &mut operation)
        .await
        .expect("ordinary output watchdog")
        .expect("ordinary compile outcome");
    drop(operation);
    fixture.cleanup().await;
    assert_eq!(result.status, ExecutionStatus::CompileFailed, "ordinary compiler Output control");
}

#[tokio::test(flavor = "current_thread")]
async fn host_pending_stdin_after_leader_exit_remains_timeout() {
    if supervised("host_pending_stdin_after_leader_exit_remains_timeout").await {
        return;
    }
    let mut fixture = Fixture::new("install", true).await;
    let executor = executor(&fixture);
    let work_timeout = Duration::from_secs(5);
    let mut request = request(work_timeout);
    request.stdin = Some(vec![b'x'; 10 * 1024 * 1024]);
    let mut operation = Box::pin(executor.execute(request));
    let ready = tokio::select! {
        ready = fixture.message() => ready,
        result = &mut operation => panic!("public binary returned before readiness: {result:?}"),
    };
    assert_eq!(ready["mode"], "stdin");
    assert_eq!(fixture.command(b"E\n").await["leader_exit_requested"], true);
    assert_eq!(fixture.command(b"P\n").await["source_exists"], true);
    let before_timeout = fixture.observed(&ready);
    assert_eq!(
        before_timeout["alive"], true,
        "F1 control descendant remains alive before work expiry"
    );
    tokio::time::pause();
    tokio::time::advance(work_timeout + Duration::from_secs(1)).await;
    tokio::time::resume(); // No await may occur before resume: kernel settlement is real time.
    let result = tokio::time::timeout(WATCHDOG, &mut operation)
        .await
        .expect("pending stdin control watchdog")
        .expect("pending stdin public outcome");
    drop(operation);
    fixture.observed(&ready);
    fixture.cleanup().await;
    assert_eq!(
        result.status,
        ExecutionStatus::Timeout,
        "F1_PRESERVATION_CONTROL: unread inherited stdin must not turn into Success"
    );
}

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use adk_code::{ContainerCommandExecutor, ContainerConfig, RustExecutor, RustExecutorConfig};
use adk_sandbox::{
    BackendCapabilities, ExecRequest, ExecResult, ProcessBackend, SandboxBackend, SandboxError,
};

#[derive(Clone, Copy)]
enum Pipeline {
    Sandbox,
    HostCompiler,
    BackendCompiler,
    ContainerCli,
}

// This wrapper selects the existing read-confining routing branch while using
// real ProcessBackend processes. It tests custody transfer, not OS confinement.
struct CompilerRoutingFixture {
    process: Arc<ProcessBackend>,
    calls: PathBuf,
}

#[async_trait::async_trait]
impl SandboxBackend for CompilerRoutingFixture {
    fn name(&self) -> &str {
        "compiler-routing-fixture"
    }

    fn capabilities(&self) -> BackendCapabilities {
        let mut capabilities = self.process.capabilities();
        capabilities.enforced_limits.filesystem_read_isolation = true;
        capabilities
    }

    async fn execute(&self, request: ExecRequest) -> Result<ExecResult, SandboxError> {
        self.process.execute(request).await
    }

    async fn execute_with_keepalive(
        self: Arc<Self>,
        request: ExecRequest,
        keepalive: Arc<dyn Send + Sync>,
    ) -> Result<ExecResult, SandboxError> {
        use std::io::Write;
        let mut calls = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.calls)
            .expect("routing receipt");
        writeln!(calls, "{}", json!({"code": request.code, "timeout": request.timeout.as_secs()}))
            .expect("record actual routing");
        self.process.clone().execute_with_keepalive(request, keepalive).await
    }
}

type PublicOperation = Pin<Box<dyn Future<Output = Result<String, String>> + Send>>;

fn public_operation(fixture: &Fixture, pipeline: Pipeline, timeout: Duration) -> PublicOperation {
    match pipeline {
        Pipeline::Sandbox => {
            let executor = executor(fixture);
            Box::pin(async move {
                executor
                    .execute(request(timeout))
                    .await
                    .map(|result| format!("{:?}", result.status))
                    .map_err(|error| error.to_string())
            })
        }
        Pipeline::HostCompiler | Pipeline::BackendCompiler => {
            let process = Arc::new(ProcessBackend::default());
            let backend: Arc<dyn SandboxBackend> = if matches!(pipeline, Pipeline::BackendCompiler)
            {
                Arc::new(CompilerRoutingFixture {
                    process,
                    calls: fixture.root.join("backend-calls.jsonl"),
                })
            } else {
                process
            };
            let dependency = fixture.root.join("libserde_json.rlib");
            std::fs::write(&dependency, b"fixture absolute dependency").expect("dependency marker");
            let executor = RustExecutor::new(
                backend,
                RustExecutorConfig {
                    rustc_path: fixture.executable.to_string_lossy().into_owned(),
                    serde_json_path: Some(dependency),
                    rustc_flags: Vec::new(),
                },
            );
            Box::pin(async move {
                executor
                    .execute(
                        "fn run(v: serde_json::Value) -> serde_json::Value { v }",
                        None,
                        timeout,
                    )
                    .await
                    .map(|result| format!("exit {}", result.exec_result.exit_code))
                    .map_err(|error| error.to_string())
            })
        }
        Pipeline::ContainerCli => {
            let executor = ContainerCommandExecutor::new(ContainerConfig {
                runtime: fixture.executable.to_string_lossy().into_owned(),
                ..ContainerConfig::default()
            });
            let mut request = request(timeout);
            request.language = ExecutionLanguage::Python;
            request.payload = ExecutionPayload::Source { code: "print('fixture')".to_string() };
            Box::pin(async move {
                executor
                    .execute(request)
                    .await
                    .map(|result| format!("{:?}", result.status))
                    .map_err(|error| error.to_string())
            })
        }
    }
}

async fn phase_shutdown(
    pipeline: Pipeline,
    mode: &str,
    closed: bool,
    internal_timeout: bool,
    metadata: bool,
) {
    let mut fixture = Fixture::new(mode, closed).await;
    let work_timeout = Duration::from_secs(5);
    let mut operation = public_operation(&fixture, pipeline, work_timeout);
    let ready = tokio::select! {
        ready = fixture.message() => ready,
        result = &mut operation => panic!("public phase returned before real readiness: {result:?}"),
    };
    assert_eq!(ready["metadata"], metadata, "actual compiler phase argv");
    assert_eq!(fixture.command(b"P\n").await["source_sha"], ready["source_sha"]);
    let before = fixture.observed(&ready);
    assert_eq!(before["alive"], true);
    assert_eq!(before["source_exists"], true);
    assert_eq!(before["output_directory_exists"], true);
    let deadline = tokio::time::Instant::now() + WATCHDOG;
    let outcome = if internal_timeout {
        // Ready first; advance only the work clock, then immediately restore real
        // time before awaiting real child/group settlement or any other future.
        tokio::time::pause();
        tokio::time::advance(work_timeout + Duration::from_secs(1)).await;
        tokio::time::resume();
        Some(
            tokio::time::timeout_at(deadline, &mut operation).await.expect("work timeout watchdog"),
        )
    } else {
        None
    };
    drop(operation);
    let observed = fixture.observed(&ready);
    let violation = observed["alive"] == true
        && (observed["source_exists"] == false || observed["output_directory_exists"] == false);
    let settled = violation || fixture.natural_settlement(&ready, deadline).await;
    let mut released = true;
    if !matches!(pipeline, Pipeline::ContainerCli) {
        for path in [
            Path::new(ready["source"].as_str().expect("source path")),
            Path::new(ready["output"].as_str().expect("output path"))
                .parent()
                .expect("output directory"),
        ] {
            released &= violation || wait_for_artifact_release(path, deadline).await;
        }
    }
    let routed = if matches!(pipeline, Pipeline::BackendCompiler) {
        let calls = std::fs::read_to_string(fixture.root.join("backend-calls.jsonl"))
            .expect("actual backend routing receipt");
        eprintln!("COMPILER_BACKEND_ROUTING {calls}");
        calls.contains(fixture.executable.to_string_lossy().as_ref())
    } else {
        true
    };
    if matches!(pipeline, Pipeline::ContainerCli) {
        let argv = ready["argv"].as_array().expect("actual CLI argv");
        assert_eq!(argv.get(1).and_then(Value::as_str), Some("run"));
        assert!(argv.iter().any(|arg| arg == "--rm"));
        assert!(argv.iter().any(|arg| arg == "-i"));
    }
    fixture.cleanup().await;
    assert!(!violation, "live original phase lost caller resources: {observed}");
    assert!(
        settled && released,
        "phase must naturally settle and release before emergency cleanup"
    );
    assert!(routed, "read-routing branch must actually transfer the compiler request");
    if let Some(outcome) = outcome {
        eprintln!("PUBLIC_PHASE_TIMEOUT_OUTCOME {outcome:?}");
        assert!(
            matches!(&outcome, Ok(status) if status == "Timeout")
                || matches!(&outcome, Err(error) if error.contains("timed out")),
            "original timeout classification must remain: {outcome:?}"
        );
    }
}

macro_rules! phase_case {
    ($name:ident, $pipeline:ident, $mode:literal, $closed:literal, $timeout:literal, $metadata:literal) => {
        #[tokio::test(flavor = "current_thread")]
        async fn $name() {
            if supervised(stringify!($name)).await {
                return;
            }
            phase_shutdown(Pipeline::$pipeline, $mode, $closed, $timeout, $metadata).await;
        }
    };
}

phase_case!(executor_check_cancel_inherited_stdio, HostCompiler, "cancel", false, false, true);
phase_case!(executor_check_cancel_closed_stdio, HostCompiler, "cancel", true, false, true);
phase_case!(executor_build_cancel_inherited_stdio, HostCompiler, "build", false, false, false);
phase_case!(executor_build_cancel_closed_stdio, HostCompiler, "build", true, false, false);
phase_case!(sandbox_compile_timeout_inherited_stdio, Sandbox, "cancel", false, true, false);
phase_case!(sandbox_compile_timeout_closed_stdio, Sandbox, "cancel", true, true, false);
phase_case!(executor_check_timeout_inherited_stdio, HostCompiler, "cancel", false, true, true);
phase_case!(executor_check_timeout_closed_stdio, HostCompiler, "cancel", true, true, true);
phase_case!(executor_build_timeout_inherited_stdio, HostCompiler, "build", false, true, false);
phase_case!(executor_build_timeout_closed_stdio, HostCompiler, "build", true, true, false);
phase_case!(sandbox_binary_cancel_inherited_stdio, Sandbox, "binary", false, false, false);
phase_case!(sandbox_binary_cancel_closed_stdio, Sandbox, "binary", true, false, false);
phase_case!(sandbox_binary_timeout_inherited_stdio, Sandbox, "binary", false, true, false);
phase_case!(sandbox_binary_timeout_closed_stdio, Sandbox, "binary", true, true, false);
phase_case!(container_cli_cancel_closed_stdio, ContainerCli, "cli", true, false, false);
phase_case!(container_cli_timeout_inherited_stdio, ContainerCli, "cli", false, true, false);
phase_case!(
    backend_check_cancel_retains_both_directories,
    BackendCompiler,
    "cancel",
    true,
    false,
    true
);
phase_case!(backend_build_cancel_retains_artifacts, BackendCompiler, "build", false, false, false);
phase_case!(
    backend_check_timeout_retains_both_directories,
    BackendCompiler,
    "cancel",
    false,
    true,
    true
);
phase_case!(executor_binary_cancel_retains_artifacts, HostCompiler, "binary", true, false, false);
phase_case!(executor_binary_timeout_retains_artifacts, HostCompiler, "binary", false, true, false);

#[tokio::test(flavor = "current_thread")]
async fn executor_normal_check_output_waits_for_descendant_source_use() {
    if supervised("executor_normal_check_output_waits_for_descendant_source_use").await {
        return;
    }
    let mut fixture = Fixture::new("normal", false).await;
    let mut operation = public_operation(&fixture, Pipeline::HostCompiler, Duration::from_secs(30));
    let ready = tokio::select! {
        ready = fixture.message() => ready,
        result = &mut operation => panic!("check returned before readiness: {result:?}"),
    };
    assert_eq!(ready["metadata"], true);
    assert_eq!(fixture.command(b"E\n").await["leader_exit_observed"], true);
    let observed = fixture.observed(&ready);
    let ack = tokio::select! {
        ack = fixture.command(b"P\n") => ack,
        result = &mut operation => panic!("check returned before descendant EOF: {result:?}"),
    };
    assert_eq!(ack["source_sha"], ready["source_sha"]);
    assert_eq!(observed["output_directory_exists"], true);
    assert_eq!(fixture.command(b"R\n").await["source_exists"], true);
    let result =
        tokio::time::timeout(WATCHDOG, &mut operation).await.expect("normal check watchdog");
    drop(operation);
    fixture.cleanup().await;
    assert!(result.is_err_and(|error| error.contains("fixture compiler failure")));
}

struct PhysicalDropProbe {
    directory: Option<tempfile::TempDir>,
    identities: PathBuf,
    pipes: Arc<std::sync::Mutex<Vec<(i32, PathBuf)>>>,
    dropped: Option<tokio::sync::oneshot::Sender<Value>>,
}

impl Drop for PhysicalDropProbe {
    fn drop(&mut self) {
        let directory = self.directory.take().expect("physical probe directory");
        let source = directory.path().join("main.rs");
        let source_held = source.exists();
        let originals: Vec<Value> = match std::fs::read_to_string(&self.identities) {
            Ok(records) => records.lines().map(|line| {
                let identity: Value = match serde_json::from_str(line) {
                    Ok(identity) => identity,
                    Err(error) => return json!({"observation_error": error.to_string()}),
                };
                let Some(pid) = identity["pid"].as_u64() else {
                    return json!({"observation_error": "missing original PID"});
                };
                let present = match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
                    Ok(raw) => {
                        let Some(start) = raw.rsplit_once(") ")
                            .and_then(|(_, fields)| fields.split_whitespace().nth(19))
                            .and_then(|start| start.parse::<u64>().ok()) else {
                            return json!({"observation_error": "malformed process identity"});
                        };
                        let Some(expected) = identity["start"].as_u64() else {
                            return json!({"observation_error": "missing original start identity"});
                        };
                        start == expected
                    },
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
                    Err(error) => return json!({"observation_error": error.to_string()}),
                };
                json!({"identity": identity, "original_present": present})
            }).collect(),
            Err(error) => vec![json!({"observation_error": error.to_string()})],
        };
        let pipes: Vec<Value> = match self.pipes.lock() {
            Ok(pipes) => pipes.iter().map(|(fd, expected)| {
                let current = match std::fs::read_link(format!("/proc/self/fd/{fd}")) {
                    Ok(current) => Some(current),
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
                    Err(error) => return json!({"observation_error": error.to_string()}),
                };
                json!({"fd": fd, "expected": expected, "original_open": current.as_ref() == Some(expected)})
            }).collect(),
            Err(error) => vec![json!({"observation_error": error.to_string()})],
        };
        drop(directory);
        let receipt = json!({"source_held_before_drop": source_held, "source_exists_after_drop": source.exists(), "originals": originals, "pipes": pipes});
        eprintln!("PHYSICAL_RESOURCE_DROP {receipt}");
        if let Some(dropped) = self.dropped.take() {
            let _ = dropped.send(receipt);
        }
    }
}

#[tokio::test(flavor = "current_thread")]
async fn physical_owner_drops_artifacts_after_actual_instances_and_pipes() {
    use std::os::fd::AsRawFd;

    use adk_sandbox::process_custody::{CompletionBoundary, run_owned_command};

    if supervised("physical_owner_drops_artifacts_after_actual_instances_and_pipes").await {
        return;
    }
    let mut fixture = Fixture::new("cancel", false).await;
    let directory = tempfile::tempdir().expect("physical owner directory");
    let source = directory.path().join("main.rs");
    std::fs::write(&source, b"source owned until physical settlement").expect("actual source");
    let mut command = Command::new(&fixture.executable);
    command
        .arg(&source)
        .arg("-o")
        .arg(directory.path().join("main"))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let pipes = Arc::new(std::sync::Mutex::new(Vec::new()));
    let captured = pipes.clone();
    let (dropped, drop_notice) = tokio::sync::oneshot::channel();
    let guard = PhysicalDropProbe {
        directory: Some(directory),
        identities: fixture.root.join("identities.jsonl"),
        pipes,
        dropped: Some(dropped),
    };
    let mut operation = Box::pin(run_owned_command(
        command,
        Duration::from_secs(30),
        guard,
        CompletionBoundary::OutputEof,
        move |pipes| {
            for fd in [
                pipes.stdout.as_ref().expect("stdout pipe").as_raw_fd(),
                pipes.stderr.as_ref().expect("stderr pipe").as_raw_fd(),
            ] {
                let identity = std::fs::read_link(format!("/proc/self/fd/{fd}"))
                    .expect("actual owned pipe identity");
                captured.lock().expect("capture pipe identities").push((fd, identity));
            }
            // This cancellation control deliberately keeps all supplied handles
            // inline; the owner must drop this future before releasing the guard.
            async move {
                let _pipes = pipes;
                std::future::pending::<std::io::Result<()>>().await
            }
        },
    ));
    let ready = tokio::select! {
        ready = fixture.message() => ready,
        result = &mut operation => panic!("owner returned before readiness: {result:?}"),
    };
    let deadline = tokio::time::Instant::now() + WATCHDOG;
    drop(operation);
    let observed = fixture.observed(&ready);
    let settled = fixture.natural_settlement(&ready, deadline).await;
    let receipt = tokio::time::timeout_at(deadline, drop_notice)
        .await
        .expect("actual physical guard drop watchdog")
        .expect("actual guard drop notice");
    fixture.cleanup().await;
    assert!(!(observed["alive"] == true && observed["source_exists"] == false));
    assert!(settled, "natural instance death before emergency cleanup");
    assert_eq!(receipt["source_held_before_drop"], true);
    assert_eq!(receipt["source_exists_after_drop"], false);
    let originals = receipt["originals"].as_array().expect("instance observations");
    assert_eq!(originals.len(), 2);
    assert!(
        originals
            .iter()
            .all(|row| row["observation_error"].is_null() && row["original_present"] == false)
    );
    let pipes = receipt["pipes"].as_array().expect("pipe observations");
    assert_eq!(pipes.len(), 2);
    assert!(
        pipes.iter().all(|row| row["observation_error"].is_null() && row["original_open"] == false)
    );
}

#[tokio::test]
async fn missing_binary_and_cli_preserve_original_spawn_error_contexts() {
    let directory = tempfile::tempdir().expect("spawn error fixture directory");
    let compiler = directory.path().join("compiler");
    std::fs::write(&compiler, b"#!/bin/sh\nexit 0\n").expect("successful compiler without binary");
    std::fs::set_permissions(&compiler, std::fs::Permissions::from_mode(0o700))
        .expect("fixture executable");
    let dependency = directory.path().join("libserde_json.rlib");
    std::fs::write(&dependency, b"absolute dependency marker").expect("dependency marker");
    let executor = RustSandboxExecutor::new(RustSandboxConfig {
        rustc_path: compiler.to_string_lossy().into_owned(),
        rustc_flags: Vec::new(),
        serde_json_path: Some(dependency),
    });
    let error =
        executor.execute(request(Duration::from_secs(5))).await.expect_err("missing binary");
    let missing = std::io::Error::from_raw_os_error(2).to_string();
    assert_eq!(
        error,
        adk_code::ExecutionError::ExecutionFailed(format!("failed to spawn binary: {missing}"))
    );
    let runtime = directory.path().join("nonexistent-runtime");
    let executor = ContainerCommandExecutor::new(ContainerConfig {
        runtime: runtime.to_string_lossy().into_owned(),
        ..ContainerConfig::default()
    });
    let mut request = request(Duration::from_secs(5));
    request.language = ExecutionLanguage::Python;
    let error = executor.execute(request).await.expect_err("missing CLI runtime");
    assert_eq!(
        error,
        adk_code::ExecutionError::ExecutionFailed(format!(
            "failed to spawn container runtime '{}': {missing}",
            runtime.display()
        ))
    );
}

#[tokio::test]
async fn nonzero_cli_exit_remains_a_completed_execution_result() {
    let executor = ContainerCommandExecutor::new(ContainerConfig {
        runtime: "/bin/sh".to_string(),
        ..ContainerConfig::default()
    });
    // The real shell rejects the preserved Docker argv. This is a normal
    // nonzero process exit, and must not become a spawn/collection I/O error.
    let mut request = request(Duration::from_secs(5));
    request.language = ExecutionLanguage::Python;
    let result = executor.execute(request).await.expect("nonzero completed CLI");
    assert_eq!(result.status, ExecutionStatus::Failed);
    assert!(result.exit_code.is_some_and(|code| code != 0));
    assert!(!result.stderr.is_empty());
}
