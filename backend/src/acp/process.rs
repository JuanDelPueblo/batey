use process_wrap::tokio::TokioChildWrapper;
use std::collections::HashMap;
use std::path::Path;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::{ChildStderr, ChildStdin, ChildStdout};
use tokio::sync::mpsc::UnboundedSender;

#[cfg(not(windows))]
use process_wrap::tokio::{KillOnDrop, ProcessGroup, TokioCommandWrap};
#[cfg(not(windows))]
use std::process::Stdio;
#[cfg(not(windows))]
use tokio::process::Command;

/// Distinguishes an `execve` `ENOENT` caused by a missing executable from one
/// caused by a missing ELF interpreter/loader for an executable that does
/// exist. The OS reports both the same way (`std::io::ErrorKind::NotFound`),
/// but they need different fixes: install the agent, versus fix the runtime
/// (see T129 -- a Registry-installed native binary with a conventional
/// interpreter path the OCI image did not yet provide).
#[cfg(not(windows))]
fn classify_spawn_error(
    command: &str,
    env_vars: &HashMap<String, String>,
    e: std::io::Error,
) -> anyhow::Error {
    if e.kind() != std::io::ErrorKind::NotFound {
        return anyhow::anyhow!("Failed to spawn ACP agent '{command}': {e}");
    }
    let search_path = env_vars
        .get("PATH")
        .map(std::ffi::OsString::from)
        .unwrap_or_default();
    match crate::agents::which_in(command, &search_path) {
        Some(_) => anyhow::anyhow!(
            "Failed to spawn ACP agent '{command}': the executable exists but the process \
             could not start, commonly a missing ELF interpreter/loader for this binary: {e}"
        ),
        None => anyhow::anyhow!("Failed to spawn ACP agent '{command}': executable not found"),
    }
}

pub struct AcpProcess {
    pub child: Box<dyn TokioChildWrapper>,
    pub root_pid: Option<u32>,
    pub stdin: ChildStdin,
    pub stdout: BufReader<ChildStdout>,
    pub stderr: ChildStderr,
}

impl AcpProcess {
    pub fn spawn(
        command: &str,
        args: &[String],
        env_vars: &HashMap<String, String>,
        cwd: &Path,
    ) -> anyhow::Result<Self> {
        // Windows: use our hand-rolled JobObject path. process-wrap's `JobObject`
        // wrapper associates a completion port with the job; that association
        // empirically prevents `KILL_ON_JOB_CLOSE` from firing when batey dies,
        // leaving the codex-acp subtree alive. Confirmed via tasklist after
        // matching reproductions on both paths.
        #[cfg(windows)]
        {
            super::windows_job::spawn(command, args, env_vars, cwd)
        }

        #[cfg(not(windows))]
        {
            let mut cmd = Command::new(command);
            cmd.args(args);
            cmd.current_dir(cwd)
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped());
            cmd.env_clear();
            for (k, v) in env_vars {
                cmd.env(k, v);
            }

            // ProcessGroup::leader() makes the child the head of a new pgrp so
            // that `start_kill` (= killpg via process-wrap's ProcessGroupChild)
            // wipes the whole subtree — agents like codex-acp spawn their own
            // children, and `KillOnDrop` alone (tokio kill_on_drop = TerminateProcess
            // on the direct child only) would orphan them on Linux/macOS.
            let mut wrap = TokioCommandWrap::from(cmd);
            wrap.wrap(ProcessGroup::leader());
            wrap.wrap(KillOnDrop);

            let mut child = wrap
                .spawn()
                .map_err(|e| classify_spawn_error(command, env_vars, e))?;
            let root_pid = child.id();

            let stdin = child
                .stdin()
                .take()
                .ok_or_else(|| anyhow::anyhow!("stdin not piped"))?;
            let stdout = child
                .stdout()
                .take()
                .ok_or_else(|| anyhow::anyhow!("stdout not piped"))?;
            let stderr = child
                .stderr()
                .take()
                .ok_or_else(|| anyhow::anyhow!("stderr not piped"))?;

            Ok(Self {
                child,
                root_pid,
                stdin,
                stdout: BufReader::new(stdout),
                stderr,
            })
        }
    }

    pub async fn kill(&mut self) {
        let _ = Box::into_pin(self.child.kill()).await;
    }
}

/// What one ACP process may write to the server log through stderr.
///
/// Ordinary chat agents use `Log`, because their diagnostics help an
/// operator. Authentication processes use `Discard` by default: their stderr
/// frequently carries device codes, URLs, tokens, or other credentials, and
/// that material must never reach the log. The compatibility capture mode is
/// an exception only for its narrowly recognized URL candidate; `Discard`
/// keeps draining the pipe, so the child never blocks on a full stderr buffer.
#[derive(Debug, Clone)]
pub enum StderrPolicy {
    /// Log every non-empty stderr line at warning level.
    Log,
    /// Drain stderr but never log a line from it.
    Discard,
    /// Drain stderr and forward only a narrowly recognized HTTPS OAuth URL
    /// candidate. All other stderr remains discard-only.
    CaptureAuthUrl(UnboundedSender<String>),
}

/// How many of the most recent stderr lines `StderrTail` keeps, and how much
/// of each line, so a startup-failure diagnostic stays bounded.
const STDERR_TAIL_MAX_LINES: usize = 5;
const STDERR_TAIL_MAX_LINE_CHARS: usize = 200;

/// A small bounded ring of the most recent stderr lines from one ACP
/// process, populated only under `StderrPolicy::Log`.
///
/// It exists so a connection that closes before ACP initialization completes
/// (for example, a native module failing to load its shared libraries) can
/// report *why* instead of a bare "connection closed". It is never populated
/// for authentication processes, which use `Discard` or `CaptureAuthUrl`, so
/// no credential-bearing stderr is retained here, matching the same rule
/// `drain_stderr` already applies to the log itself.
#[derive(Clone, Default)]
pub struct StderrTail(std::sync::Arc<std::sync::Mutex<std::collections::VecDeque<String>>>);

impl StderrTail {
    pub fn new() -> Self {
        Self::default()
    }

    fn push(&self, line: &str) {
        let mut truncated = line.to_string();
        truncated.truncate(STDERR_TAIL_MAX_LINE_CHARS);
        let mut buf = self.0.lock().expect("stderr tail lock");
        if buf.len() == STDERR_TAIL_MAX_LINES {
            buf.pop_front();
        }
        buf.push_back(truncated);
    }

    /// The captured lines joined into one diagnostic snippet, or `None` when
    /// nothing was captured.
    pub fn snippet(&self) -> Option<String> {
        let buf = self.0.lock().expect("stderr tail lock");
        if buf.is_empty() {
            return None;
        }
        Some(buf.iter().cloned().collect::<Vec<_>>().join(" | "))
    }
}

pub async fn drain_stderr(
    stderr: ChildStderr,
    agent_name: String,
    policy: StderrPolicy,
    tail: StderrTail,
) {
    let mut reader = BufReader::new(stderr);
    let mut line = String::new();
    loop {
        line.clear();
        match reader.read_line(&mut line).await {
            Ok(0) => break,
            Ok(_) => {
                let trimmed = line.trim_end();
                if trimmed.is_empty() {
                    continue;
                }
                match policy {
                    StderrPolicy::Log => {
                        tracing::warn!(agent = %agent_name, "stderr: {}", trimmed);
                        tail.push(trimmed);
                    }
                    // The line may be a credential. Drop it entirely.
                    StderrPolicy::Discard => {}
                    StderrPolicy::CaptureAuthUrl(ref sender) => {
                        if let Some(url) = auth_url_candidate(trimmed) {
                            let _ = sender.send(url);
                        }
                    }
                }
            }
            Err(e) => {
                tracing::debug!(agent = %agent_name, "stderr read error: {}", e);
                break;
            }
        }
    }
}

/// Finds only a URL-shaped token that advertises both OAuth parameters used by
/// the compatibility flow. This deliberately does not retain generic URLs,
/// tokens, device codes, stack traces, or arbitrary stderr text.
fn auth_url_candidate(line: &str) -> Option<String> {
    for token in line.split_whitespace() {
        let candidate = token.trim_matches(|c: char| {
            matches!(
                c,
                '"' | '\'' | '`' | '(' | ')' | '[' | ']' | '<' | '>' | ','
            )
        });
        if !candidate.starts_with("https://") {
            continue;
        }
        let parsed = match url::Url::parse(candidate) {
            Ok(url) => url,
            Err(_) => continue,
        };
        let mut has_redirect = false;
        let mut has_state = false;
        for (name, _) in parsed.query_pairs() {
            has_redirect |= name == "redirect_uri";
            has_state |= name == "state";
        }
        if has_redirect && has_state {
            return Some(candidate.to_owned());
        }
    }
    None
}

#[cfg(all(test, unix))]
mod stderr_policy_tests {
    use super::*;
    use std::process::Stdio;
    use std::sync::{Arc, Mutex};

    /// A shared in-memory sink for tracing events.
    #[derive(Clone, Default)]
    struct LogBuffer(Arc<Mutex<Vec<u8>>>);

    impl std::io::Write for LogBuffer {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0
                .lock()
                .expect("log buffer lock")
                .extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for LogBuffer {
        type Writer = LogBuffer;
        fn make_writer(&'a self) -> Self::Writer {
            self.clone()
        }
    }

    /// Writes one secret marker to stderr, then one more line so the marker
    /// is never the final buffered tail, and runs the drain to completion.
    fn drain_marker_stderr(policy: StderrPolicy, marker: &str) -> String {
        let script = format!("echo {marker} >&2; echo after-secret >&2");
        let buffer = LogBuffer::default();
        let subscriber = tracing_subscriber::fmt()
            .with_ansi(false)
            .with_writer(buffer.clone())
            .finish();
        // The drain runs inline on this thread, so a scoped subscriber
        // captures exactly what this policy emits.
        tracing::subscriber::with_default(subscriber, || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("a runtime for the drain");
            runtime.block_on(async {
                let mut child = tokio::process::Command::new("sh")
                    .arg("-c")
                    .arg(script)
                    .stderr(Stdio::piped())
                    .spawn()
                    .expect("a process that writes stderr");
                let stderr = child.stderr.take().expect("stderr was piped");
                drain_stderr(stderr, "policy-probe".to_owned(), policy, StderrTail::new()).await;
                child.wait().await.expect("the stderr writer exited");
            });
        });
        let captured = buffer.0.lock().expect("log buffer lock").clone();
        String::from_utf8(captured).expect("the log buffer is valid UTF-8")
    }

    /// Authentication stderr may carry credentials. The discard policy keeps
    /// draining it, but nothing from it reaches the log.
    #[test]
    fn discarded_stderr_never_reaches_the_log() {
        const SECRET: &str = "DEVICE-CODE-SECRET-7f3a";
        let logged = drain_marker_stderr(StderrPolicy::Discard, SECRET);
        assert!(
            !logged.contains(SECRET),
            "discarded stderr reached the log: {logged}"
        );
    }

    #[test]
    fn auth_stderr_capture_accepts_only_oauth_url_candidates() {
        let valid = "https://accounts.example.test/authorize?redirect_uri=http%3A%2F%2F127.0.0.1%3A43123%2Fcallback&state=opaque";
        assert_eq!(
            auth_url_candidate(&format!("opening {valid}")),
            Some(valid.into())
        );
        for line in [
            "device code: ABCD-EFGH",
            "https://accounts.example.test/help",
            "panic: bearer-token-secret",
            "not-a-url https://accounts.example.test/authorize?state=opaque",
        ] {
            assert_eq!(
                auth_url_candidate(line),
                None,
                "captured unrelated stderr: {line}"
            );
        }
    }

    /// Ordinary chat-agent stderr keeps reaching the log, so the discard
    /// policy must never silently become the default.
    #[test]
    fn logged_stderr_still_reaches_the_log() {
        const DIAGNOSTIC: &str = "CHAT-AGENT-DIAGNOSTIC-2b9d";
        let logged = drain_marker_stderr(StderrPolicy::Log, DIAGNOSTIC);
        assert!(
            logged.contains(DIAGNOSTIC),
            "ordinary agent stderr stopped being logged: {logged}"
        );
    }

    /// A startup-failure diagnostic (T129) needs the tail captured under
    /// `Log`, so a launch failure like a missing shared library is visible
    /// beyond a bare "connection closed".
    #[test]
    fn log_policy_populates_the_stderr_tail() {
        let script = "echo error: cannot open shared object file >&2";
        let tail = StderrTail::new();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("a runtime for the drain");
        runtime.block_on(async {
            let mut child = tokio::process::Command::new("sh")
                .arg("-c")
                .arg(script)
                .stderr(Stdio::piped())
                .spawn()
                .expect("a process that writes stderr");
            let stderr = child.stderr.take().expect("stderr was piped");
            drain_stderr(
                stderr,
                "tail-probe".to_owned(),
                StderrPolicy::Log,
                tail.clone(),
            )
            .await;
            child.wait().await.expect("the stderr writer exited");
        });
        assert_eq!(
            tail.snippet(),
            Some("error: cannot open shared object file".to_string())
        );
    }

    /// Authentication stderr may carry credentials, so the tail must stay
    /// empty under `Discard` exactly as the log does.
    #[test]
    fn discard_policy_never_populates_the_stderr_tail() {
        const SECRET: &str = "DEVICE-CODE-SECRET-9c1a";
        let tail = StderrTail::new();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("a runtime for the drain");
        runtime.block_on(async {
            let mut child = tokio::process::Command::new("sh")
                .arg("-c")
                .arg(format!("echo {SECRET} >&2"))
                .stderr(Stdio::piped())
                .spawn()
                .expect("a process that writes stderr");
            let stderr = child.stderr.take().expect("stderr was piped");
            drain_stderr(
                stderr,
                "tail-probe".to_owned(),
                StderrPolicy::Discard,
                tail.clone(),
            )
            .await;
            child.wait().await.expect("the stderr writer exited");
        });
        assert_eq!(tail.snippet(), None);
    }

    /// The tail keeps only the most recent lines, bounded, so a chatty
    /// process cannot turn a diagnostic into an unbounded log.
    #[test]
    fn stderr_tail_is_bounded() {
        let tail = StderrTail::new();
        for i in 0..(STDERR_TAIL_MAX_LINES + 2) {
            tail.push(&format!("line-{i}"));
        }
        let snippet = tail.snippet().expect("some lines were pushed");
        assert_eq!(snippet.matches('|').count(), STDERR_TAIL_MAX_LINES - 1);
        assert!(
            !snippet.contains("line-0"),
            "oldest line should have been evicted: {snippet}"
        );

        let long_line = "x".repeat(STDERR_TAIL_MAX_LINE_CHARS * 3);
        let tail = StderrTail::new();
        tail.push(&long_line);
        let snippet = tail.snippet().expect("a line was pushed");
        assert_eq!(snippet.len(), STDERR_TAIL_MAX_LINE_CHARS);
    }

    /// A command that plainly does not exist is reported as such, not as
    /// some other spawn failure.
    #[test]
    fn missing_executable_is_reported_as_missing() {
        let env = HashMap::from([("PATH".to_string(), "/nonexistent-batey-test".to_string())]);
        let result = AcpProcess::spawn(
            "batey-test-definitely-absent",
            &[],
            &env,
            std::path::Path::new("/"),
        );
        let message = match result {
            Ok(_) => panic!("the command does not exist"),
            Err(e) => e.to_string(),
        };
        assert!(
            message.contains("executable not found"),
            "unexpected message: {message}"
        );
    }

    /// The regression this guards (T129): an executable that exists but
    /// whose ELF interpreter/loader is missing fails with the same OS-level
    /// `ENOENT` as a missing executable. The distinction must survive that,
    /// so an operator does not chase the wrong fix. Needs `cc` and
    /// `patchelf`, both present in the Nix dev shell and `nix run .#verify`;
    /// it skips itself where they are not (for example, a bare CI runner
    /// with no `patchelf`), since `nix/oci-runtime-compat-smoke.sh` already
    /// exercises this exact failure mode end to end against a real image.
    #[test]
    fn existing_binary_with_missing_interpreter_is_distinguished() {
        let empty = std::ffi::OsString::new();
        if crate::agents::which_in("cc", &empty).is_none()
            && crate::agents::which_in("gcc", &empty).is_none()
        {
            eprintln!("skipping: no C compiler on PATH");
            return;
        }
        let Ok(path) = std::env::var("PATH") else {
            eprintln!("skipping: no PATH to search for cc/patchelf");
            return;
        };
        let path = std::ffi::OsString::from(path);
        let cc =
            crate::agents::which_in("cc", &path).or_else(|| crate::agents::which_in("gcc", &path));
        let Some(cc) = cc else {
            eprintln!("skipping: no C compiler on PATH");
            return;
        };
        let Some(patchelf) = crate::agents::which_in("patchelf", &path) else {
            eprintln!("skipping: no patchelf on PATH");
            return;
        };

        let dir = std::env::temp_dir().join(format!("batey-interp-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("a scratch directory");
        let source = dir.join("hello.c");
        std::fs::write(&source, "int main(void) { return 0; }\n").expect("wrote source");
        let binary = dir.join("hello");
        let compiled = std::process::Command::new(&cc)
            .arg(&source)
            .arg("-o")
            .arg(&binary)
            .status()
            .expect("ran the compiler");
        assert!(compiled.success(), "test binary failed to compile");
        let patched = std::process::Command::new(&patchelf)
            .arg("--set-interpreter")
            .arg("/nonexistent-batey-test-loader")
            .arg(&binary)
            .status()
            .expect("ran patchelf");
        assert!(
            patched.success(),
            "patchelf failed to repoint the interpreter"
        );

        let env = HashMap::new();
        let result = AcpProcess::spawn(
            binary.to_str().expect("a utf-8 path"),
            &[],
            &env,
            std::path::Path::new("/"),
        );
        std::fs::remove_dir_all(&dir).ok();

        let message = match result {
            Ok(_) => panic!("a binary with a missing interpreter cannot start"),
            Err(e) => e.to_string(),
        };
        assert!(
            message.contains("missing ELF interpreter/loader"),
            "unexpected message: {message}"
        );
        assert!(
            !message.contains("executable not found"),
            "an existing file was misreported as missing: {message}"
        );
    }
}
