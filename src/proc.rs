//! Subprocess plumbing: spawn with a hang-guard timeout and a cancellation token, stream
//! stdout line by line, kill the whole process tree on cancel/timeout, resolve `.cmd`
//! shims on Windows, and scrub the environment for untrusted commands.

use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};
use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::process::{Child, Command};
use tokio_util::sync::CancellationToken;

#[derive(Debug, Clone)]
pub struct CommandSpec {
    pub program: String,
    pub args: Vec<String>,
    pub cwd: PathBuf,
    /// Extra environment variables.
    pub env: Vec<(String, String)>,
    /// Start from an empty environment plus `env` (plus the allow-list from `scrubbed_env`).
    pub clear_env: bool,
    pub stdin: Option<String>,
}

impl CommandSpec {
    pub fn new(program: impl Into<String>, cwd: impl Into<PathBuf>) -> Self {
        Self {
            program: program.into(),
            args: Vec::new(),
            cwd: cwd.into(),
            env: Vec::new(),
            clear_env: false,
            stdin: None,
        }
    }
    pub fn arg(mut self, a: impl Into<String>) -> Self {
        self.args.push(a.into());
        self
    }
    pub fn args<I, S>(mut self, it: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.args.extend(it.into_iter().map(Into::into));
        self
    }
    pub fn env(mut self, k: impl Into<String>, v: impl Into<String>) -> Self {
        self.env.push((k.into(), v.into()));
        self
    }
    pub fn stdin(mut self, s: impl Into<String>) -> Self {
        self.stdin = Some(s.into());
        self
    }
    /// Untrusted command (repro, verify): minimal environment, no provider keys.
    pub fn scrubbed(mut self) -> Self {
        self.clear_env = true;
        for (k, v) in scrubbed_env() {
            self.env.push((k, v));
        }
        self
    }
    /// Shell-ish command line split on whitespace (`cargo test --workspace`).
    pub fn from_line(line: &str, cwd: impl Into<PathBuf>) -> Result<Self> {
        let mut parts = line.split_whitespace();
        let program = parts.next().context("empty command line")?;
        Ok(Self::new(program, cwd).args(parts.map(|s| s.to_string())))
    }
    /// Human-readable command line for logs.
    pub fn display(&self) -> String {
        let mut s = self.program.clone();
        for a in &self.args {
            s.push(' ');
            if a.contains(' ') || a.is_empty() {
                s.push('"');
                s.push_str(&a.replace('"', "\\\""));
                s.push('"');
            } else {
                s.push_str(a);
            }
        }
        s
    }
}

#[derive(Debug, Clone, Default)]
pub struct CommandOutput {
    pub exit_code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
    pub timed_out: bool,
    pub cancelled: bool,
    /// Output was too large to retain safely; never use partial output as test evidence.
    pub output_truncated: bool,
    /// A descendant kept an inherited output pipe open after the direct child exited.
    pub drain_timed_out: bool,
    pub duration: Duration,
}

impl CommandOutput {
    pub fn success(&self) -> bool {
        self.exit_code == Some(0)
            && !self.timed_out
            && !self.cancelled
            && !self.output_truncated
            && !self.drain_timed_out
    }
    /// stdout followed by stderr, for logs.
    pub fn combined(&self) -> String {
        if self.stderr.is_empty() {
            self.stdout.clone()
        } else {
            format!("{}\n--- stderr ---\n{}", self.stdout, self.stderr)
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum StreamLine {
    Stdout(String),
    Stderr(String),
}

/// Environment allow-list for untrusted commands.
pub fn scrubbed_env() -> Vec<(String, String)> {
    const KEEP: &[&str] = &[
        "PATH",
        "HOME",
        "USERPROFILE",
        "HOMEDRIVE",
        "HOMEPATH",
        "TEMP",
        "TMP",
        "TMPDIR",
        "SYSTEMROOT",
        "SystemRoot",
        "COMSPEC",
        "ComSpec",
        "APPDATA",
        "LOCALAPPDATA",
        "PROGRAMDATA",
        "PATHEXT",
        "CARGO_HOME",
        "RUSTUP_HOME",
        "RUSTUP_TOOLCHAIN",
        "CARGO_TARGET_DIR",
        "LANG",
        "LC_ALL",
        "TERM",
        "USER",
        "USERNAME",
        "SHELL",
    ];
    std::env::vars().filter(|(k, _)| KEEP.iter().any(|keep| keep.eq_ignore_ascii_case(k))).collect()
}

/// Keep only the runtime's authentication variables in addition to the ordinary toolchain
/// environment. This does not isolate files in HOME; tool permissions and the runtime's
/// sandbox remain responsible for filesystem access.
pub fn provider_env(keys: &[&str]) -> Vec<(String, String)> {
    let mut env = scrubbed_env();
    env.extend(std::env::vars().filter(|(k, _)| keys.iter().any(|key| key.eq_ignore_ascii_case(k))));
    env
}

/// Finds `name` on PATH. On Windows this resolves npm shims (`claude.cmd`); Rust's std runs
/// `.cmd`/`.bat` files through `cmd.exe` with proper escaping, so the path can be spawned directly.
pub fn resolve_program(name: &str) -> Result<PathBuf> {
    let p = Path::new(name);
    if p.components().count() > 1 && p.exists() {
        return Ok(p.to_path_buf());
    }
    which::which(name).with_context(|| format!("'{name}' not found on PATH"))
}

fn build(spec: &CommandSpec) -> Command {
    let mut cmd = Command::new(&spec.program);
    cmd.args(&spec.args).current_dir(&spec.cwd);
    if spec.clear_env {
        cmd.env_clear();
    }
    for (k, v) in &spec.env {
        cmd.env(k, v);
    }
    cmd.stdin(if spec.stdin.is_some() { Stdio::piped() } else { Stdio::null() })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    #[cfg(unix)]
    {
        cmd.process_group(0);
    }
    #[cfg(windows)]
    {
        use windows_sys::Win32::System::Threading::{CREATE_NO_WINDOW, CREATE_SUSPENDED};
        // Assign the Job Object before any child code can spawn an escaping descendant.
        cmd.creation_flags(CREATE_NO_WINDOW | CREATE_SUSPENDED);
    }
    cmd
}

/// Owns the process group/job independently of `Child::id()`, which disappears on reap.
/// Dropping the future therefore also kills descendants, including npm shim children.
struct ProcessTree {
    #[cfg(unix)]
    pgid: libc::pid_t,
    #[cfg(windows)]
    job: std::os::windows::io::OwnedHandle,
}

impl ProcessTree {
    fn attach(child: &Child) -> Result<Self> {
        let pid = child.id().context("spawned process has no PID")?;
        #[cfg(unix)]
        {
            Ok(Self { pgid: pid as libc::pid_t })
        }
        #[cfg(windows)]
        {
            use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
            use windows_sys::Win32::System::JobObjects::{
                AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation, SetInformationJobObject,
                JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
            };
            // SAFETY: all handles are checked, owned once, and remain alive throughout use.
            unsafe {
                let raw = CreateJobObjectW(std::ptr::null(), std::ptr::null());
                if raw.is_null() {
                    return Err(std::io::Error::last_os_error()).context("creating process job");
                }
                let job = OwnedHandle::from_raw_handle(raw);
                let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
                limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
                if SetInformationJobObject(
                    job.as_raw_handle(),
                    JobObjectExtendedLimitInformation,
                    &limits as *const _ as *const _,
                    std::mem::size_of_val(&limits) as u32,
                ) == 0
                {
                    return Err(std::io::Error::last_os_error()).context("configuring process job");
                }
                let handle = child.raw_handle().context("spawned process has no handle")?;
                if AssignProcessToJobObject(job.as_raw_handle(), handle) == 0 {
                    return Err(std::io::Error::last_os_error()).context("assigning process job");
                }
                let tree = Self { job };
                resume_suspended_process(pid)?;
                Ok(tree)
            }
        }
    }

    fn kill(&self) {
        #[cfg(unix)]
        // SAFETY: pgid is the retained positive PID of the new process group we own.
        unsafe {
            libc::killpg(self.pgid, libc::SIGKILL);
        }
        #[cfg(windows)]
        {
            use std::os::windows::io::AsRawHandle;
            // SAFETY: the owned Job Object handle remains valid for this call.
            unsafe {
                windows_sys::Win32::System::JobObjects::TerminateJobObject(self.job.as_raw_handle(), 1);
            }
        }
    }
}

impl Drop for ProcessTree {
    fn drop(&mut self) {
        self.kill();
    }
}

#[cfg(windows)]
fn resume_suspended_process(pid: u32) -> Result<()> {
    use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
    use windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;
    use windows_sys::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, Thread32First, Thread32Next, TH32CS_SNAPTHREAD, THREADENTRY32,
    };
    use windows_sys::Win32::System::Threading::{OpenThread, ResumeThread, THREAD_SUSPEND_RESUME};
    // SAFETY: snapshot/thread handles are checked and immediately placed in RAII owners.
    unsafe {
        let raw = CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0);
        if raw == INVALID_HANDLE_VALUE {
            return Err(std::io::Error::last_os_error()).context("enumerating suspended process thread");
        }
        let snapshot = OwnedHandle::from_raw_handle(raw);
        let mut entry: THREADENTRY32 = std::mem::zeroed();
        entry.dwSize = std::mem::size_of_val(&entry) as u32;
        let mut found = Thread32First(snapshot.as_raw_handle(), &mut entry);
        while found != 0 {
            if entry.th32OwnerProcessID == pid {
                let raw = OpenThread(THREAD_SUSPEND_RESUME, 0, entry.th32ThreadID);
                if raw.is_null() {
                    return Err(std::io::Error::last_os_error()).context("opening suspended process thread");
                }
                let thread = OwnedHandle::from_raw_handle(raw);
                if ResumeThread(thread.as_raw_handle()) == u32::MAX {
                    return Err(std::io::Error::last_os_error()).context("resuming process thread");
                }
                return Ok(());
            }
            found = Thread32Next(snapshot.as_raw_handle(), &mut entry);
        }
    }
    anyhow::bail!("no initial thread found for suspended process {pid}")
}

const MAX_CAPTURE_BYTES: usize = 8 * 1024 * 1024;
const MAX_LINE_BYTES: usize = 1024 * 1024;
const PIPE_DRAIN_GRACE: Duration = Duration::from_secs(2);

fn append_bounded(dst: &mut String, line: &str) -> bool {
    let remaining = MAX_CAPTURE_BYTES.saturating_sub(dst.len());
    if remaining == 0 {
        return true;
    }
    let mut end = line.len().min(remaining);
    while !line.is_char_boundary(end) {
        end -= 1;
    }
    dst.push_str(&line[..end]);
    let terminator = usize::from(!line.ends_with('\0'));
    if terminator != 0 && dst.len() < MAX_CAPTURE_BYTES {
        dst.push('\n');
    }
    end < line.len() || line.len().saturating_add(terminator) > remaining
}

enum PipeEvent {
    Line(StreamLine),
    Truncated,
    ReadError(String),
}

/// Bounded buffering even when a child emits a huge line or no newlines at all. Decode
/// only complete byte lines so a split UTF-8 sequence is preserved; invalid bytes are lossy.
async fn read_pipe<R: AsyncRead + Unpin>(mut reader: R, tx: tokio::sync::mpsc::Sender<PipeEvent>, stdout: bool) {
    let mut chunk = [0u8; 8192];
    let mut pending = Vec::new();
    let mut discarding = false;
    loop {
        let n = match reader.read(&mut chunk).await {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) => {
                let _ = tx.send(PipeEvent::ReadError(e.to_string())).await;
                return;
            }
        };
        for byte in &chunk[..n] {
            if *byte == b'\n' || *byte == 0 {
                if *byte == 0 && !discarding {
                    pending.push(0);
                }
                if !discarding && !send_pipe_line(&tx, &pending, stdout).await {
                    return;
                }
                pending.clear();
                discarding = false;
            } else if !discarding {
                if pending.len() == MAX_LINE_BYTES {
                    pending.clear();
                    discarding = true;
                    if tx.send(PipeEvent::Truncated).await.is_err() {
                        return;
                    }
                } else {
                    pending.push(*byte);
                }
            }
        }
    }
    if !pending.is_empty() && !discarding {
        let _ = send_pipe_line(&tx, &pending, stdout).await;
    }
}

async fn send_pipe_line(tx: &tokio::sync::mpsc::Sender<PipeEvent>, bytes: &[u8], stdout: bool) -> bool {
    let line = String::from_utf8_lossy(bytes).trim_end_matches('\r').to_string();
    tx.send(PipeEvent::Line(if stdout { StreamLine::Stdout(line) } else { StreamLine::Stderr(line) })).await.is_ok()
}

/// Aborting an outer run must also stop its pipe readers and blocked stdin writer.
struct IoTasks(Vec<tokio::task::JoinHandle<()>>);
impl Drop for IoTasks {
    fn drop(&mut self) {
        for task in &self.0 {
            task.abort();
        }
    }
}

/// Runs to completion, collecting stdout/stderr.
pub async fn run(spec: &CommandSpec, timeout: Duration, cancel: &CancellationToken) -> Result<CommandOutput> {
    let mut stdout = String::new();
    let mut stderr = String::new();
    let mut truncated = false;
    let mut out = run_streaming(spec, timeout, cancel, |line| match line {
        StreamLine::Stdout(l) => {
            truncated |= append_bounded(&mut stdout, &l);
        }
        StreamLine::Stderr(l) => {
            truncated |= append_bounded(&mut stderr, &l);
        }
    })
    .await?;
    out.output_truncated |= truncated;
    Ok(CommandOutput { stdout, stderr, ..out })
}

/// Bridge for synchronous git/gh call sites. A dedicated thread avoids nesting or
/// blocking the caller's Tokio runtime; cancellation still reaches the process tree.
pub fn run_blocking(spec: &CommandSpec, timeout: Duration, cancel: &CancellationToken) -> Result<CommandOutput> {
    let spec = spec.clone();
    let cancel = cancel.clone();
    std::thread::spawn(move || {
        tokio::runtime::Builder::new_current_thread().enable_all().build()?.block_on(run(&spec, timeout, &cancel))
    })
    .join()
    .map_err(|_| anyhow::anyhow!("subprocess worker panicked"))?
}

/// Runs the command, calling `on_line` for every stdout/stderr line as it arrives.
/// Returns exit status and timing; the text itself is only delivered through `on_line`.
pub async fn run_streaming<F>(
    spec: &CommandSpec,
    timeout: Duration,
    cancel: &CancellationToken,
    mut on_line: F,
) -> Result<CommandOutput>
where
    F: FnMut(StreamLine),
{
    let start = Instant::now();
    let mut cmd = build(spec);
    let mut child = cmd.spawn().with_context(|| format!("spawning `{}` in {}", spec.display(), spec.cwd.display()))?;
    let tree = ProcessTree::attach(&child)?;
    let mut tasks = IoTasks(Vec::new());

    if let (Some(input), Some(mut stdin)) = (spec.stdin.clone(), child.stdin.take()) {
        tasks.0.push(tokio::spawn(async move {
            use tokio::io::AsyncWriteExt;
            let _ = stdin.write_all(input.as_bytes()).await;
            let _ = stdin.shutdown().await;
        }));
    }

    let stdout = child.stdout.take().context("no stdout")?;
    let stderr = child.stderr.take().context("no stderr")?;
    let (tx, mut rx) = tokio::sync::mpsc::channel::<PipeEvent>(32);
    tasks.0.push(tokio::spawn(read_pipe(stdout, tx.clone(), true)));
    tasks.0.push(tokio::spawn(read_pipe(stderr, tx, false)));

    let deadline = tokio::time::sleep(timeout);
    tokio::pin!(deadline);
    let mut timed_out = false;
    let mut cancelled = false;
    let mut exit_code: Option<i32> = None;
    let mut exited = false;
    let mut pipes_closed = false;
    let mut output_truncated = false;
    let mut drain_timed_out = false;
    let mut read_error = None;
    let mut drain_deadline = None;

    while !exited || !pipes_closed {
        let drain_at = drain_deadline;
        let drain = async move {
            match drain_at {
                Some(at) => tokio::time::sleep_until(at).await,
                None => std::future::pending::<()>().await,
            }
        };
        tokio::select! {
            biased;
            _ = cancel.cancelled(), if !cancelled => {
                cancelled = true;
                tree.kill();
                let _ = child.start_kill();
                drain_deadline.get_or_insert(tokio::time::Instant::now() + PIPE_DRAIN_GRACE);
            }
            _ = &mut deadline, if !timed_out => {
                timed_out = true;
                tree.kill();
                let _ = child.start_kill();
                drain_deadline.get_or_insert(tokio::time::Instant::now() + PIPE_DRAIN_GRACE);
            }
            _ = drain => {
                drain_timed_out = true;
                tree.kill();
                let _ = child.start_kill();
                break;
            }
            status = child.wait(), if !exited => {
                exited = true;
                exit_code = status.ok().and_then(|s| s.code());
                drain_deadline.get_or_insert(tokio::time::Instant::now() + PIPE_DRAIN_GRACE);
            }
            line = rx.recv(), if !pipes_closed => {
                match line {
                    Some(PipeEvent::Line(l)) => on_line(l),
                    Some(PipeEvent::Truncated) => output_truncated = true,
                    Some(PipeEvent::ReadError(e)) => { read_error.get_or_insert(e); },
                    None => pipes_closed = true,
                }
            }
        }
    }
    tree.kill();
    if !exited {
        // Cleanup itself must remain bounded, even if the runtime or OS misbehaves.
        exit_code =
            tokio::time::timeout(PIPE_DRAIN_GRACE, child.wait()).await.ok().and_then(Result::ok).and_then(|s| s.code());
    }
    drop(tasks);
    if let Some(e) = read_error {
        anyhow::bail!("reading subprocess output: {e}");
    }
    Ok(CommandOutput {
        exit_code,
        stdout: String::new(),
        stderr: String::new(),
        timed_out,
        cancelled,
        output_truncated,
        drain_timed_out,
        duration: start.elapsed(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn echo_spec(text: &str) -> CommandSpec {
        #[cfg(windows)]
        {
            CommandSpec::new("cmd", std::env::temp_dir()).args(["/C", "echo", text])
        }
        #[cfg(not(windows))]
        {
            CommandSpec::new("echo", std::env::temp_dir()).arg(text)
        }
    }

    #[tokio::test]
    async fn runs_and_collects_output() {
        let out = run(&echo_spec("hello"), Duration::from_secs(10), &CancellationToken::new()).await.unwrap();
        assert!(out.success(), "{out:?}");
        assert!(out.stdout.contains("hello"));
    }

    #[tokio::test]
    async fn times_out_and_kills() {
        #[cfg(windows)]
        let spec = CommandSpec::new("cmd", std::env::temp_dir()).args(["/C", "ping", "-n", "30", "127.0.0.1"]);
        #[cfg(not(windows))]
        let spec = CommandSpec::new("sleep", std::env::temp_dir()).arg("30");
        let out = run(&spec, Duration::from_millis(500), &CancellationToken::new()).await.unwrap();
        assert!(out.timed_out);
        assert!(!out.success());
        assert!(out.duration < Duration::from_secs(10));
    }

    #[tokio::test]
    async fn cancel_stops_the_process() {
        #[cfg(windows)]
        let spec = CommandSpec::new("cmd", std::env::temp_dir()).args(["/C", "ping", "-n", "30", "127.0.0.1"]);
        #[cfg(not(windows))]
        let spec = CommandSpec::new("sleep", std::env::temp_dir()).arg("30");
        let token = CancellationToken::new();
        let t2 = token.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(300)).await;
            t2.cancel();
        });
        let out = run(&spec, Duration::from_secs(30), &token).await.unwrap();
        assert!(out.cancelled);
    }

    #[test]
    fn display_quotes_spaces() {
        let s = CommandSpec::new("claude", ".").args(["-p", "hello world"]);
        assert_eq!(s.display(), "claude -p \"hello world\"");
    }

    #[tokio::test]
    async fn nul_records_exceeding_line_limit_preserve_exact_output() {
        let payload = format!("{}\0", "p".repeat(1024)).repeat(2048);
        assert!(payload.len() > MAX_LINE_BYTES);
        let (tx, mut rx) = tokio::sync::mpsc::channel(4096);
        read_pipe(payload.as_bytes(), tx, true).await;
        let mut captured = String::new();
        while let Some(event) = rx.recv().await {
            match event {
                PipeEvent::Line(StreamLine::Stdout(record)) => assert!(!append_bounded(&mut captured, &record)),
                _ => panic!("complete NUL records must not truncate"),
            }
        }
        assert_eq!(captured, payload);
    }

    #[test]
    fn from_line_splits() {
        let s = CommandSpec::from_line("cargo test --workspace", ".").unwrap();
        assert_eq!(s.program, "cargo");
        assert_eq!(s.args, vec!["test", "--workspace"]);
        assert!(CommandSpec::from_line("   ", ".").is_err());
    }

    #[test]
    fn scrubbed_env_keeps_path_only_allowlist() {
        std::env::set_var("LYRA_TEST_SECRET", "x");
        let env = scrubbed_env();
        assert!(env.iter().any(|(k, _)| k.eq_ignore_ascii_case("PATH")));
        assert!(!env.iter().any(|(k, _)| k == "LYRA_TEST_SECRET"));
    }

    fn fixture_spec(mode: &str, dir: &Path) -> CommandSpec {
        CommandSpec::new(std::env::current_exe().unwrap().to_string_lossy(), dir)
            .args(["--exact", "proc::tests::subprocess_fixture", "--ignored", "--nocapture"])
            .env("LYRA_PROCESS_FIXTURE", mode)
    }

    // An inert, offline subprocess fixture. The ordinary test suite never executes it.
    #[test]
    #[ignore]
    fn subprocess_fixture() {
        use std::io::Write;
        match std::env::var("LYRA_PROCESS_FIXTURE").as_deref() {
            Ok("invalid-utf8") => {
                std::io::stdout().write_all(b"invalid: \xff\nvalid again\n").unwrap();
            }
            Ok("huge-line") => {
                std::io::stdout().write_all(&vec![b'x'; MAX_LINE_BYTES + 1]).unwrap();
                println!("\nafter huge line");
            }
            Ok("capture-limit") => {
                let line = vec![b'x'; 8192];
                for _ in 0..1100 {
                    std::io::stdout().write_all(&line).unwrap();
                    println!();
                }
            }
            Ok("descendant") => {
                std::fs::write("descendant.pid", std::process::id().to_string()).unwrap();
                std::thread::sleep(Duration::from_secs(30));
            }
            Ok("exit-with-descendant") => {
                let mut child = std::process::Command::new(std::env::current_exe().unwrap())
                    .args(["--exact", "proc::tests::subprocess_fixture", "--ignored", "--nocapture"])
                    .env("LYRA_PROCESS_FIXTURE", "descendant")
                    .spawn()
                    .unwrap();
                // Keep the descendant's inherited stdout/stderr open after this process exits.
                let limit = Instant::now() + Duration::from_secs(5);
                while !Path::new("descendant.pid").exists() && Instant::now() < limit {
                    std::thread::sleep(Duration::from_millis(10));
                }
                if !Path::new("descendant.pid").exists() {
                    let _ = child.kill();
                    panic!("descendant did not initialize");
                }
                std::process::exit(0);
            }
            other => panic!("unexpected fixture mode {other:?}"),
        }
    }

    #[tokio::test]
    async fn invalid_utf8_is_lossy_and_does_not_stop_pipe_drain() {
        let dir = tempfile::tempdir().unwrap();
        let out = run(&fixture_spec("invalid-utf8", dir.path()), Duration::from_secs(10), &CancellationToken::new())
            .await
            .unwrap();
        assert!(out.success(), "{out:?}");
        assert!(out.stdout.contains("invalid: \u{fffd}\nvalid again"));
    }

    #[tokio::test]
    async fn huge_lines_and_collected_output_are_bounded_and_rejected() {
        let dir = tempfile::tempdir().unwrap();
        for mode in ["huge-line", "capture-limit"] {
            let out =
                run(&fixture_spec(mode, dir.path()), Duration::from_secs(10), &CancellationToken::new()).await.unwrap();
            assert!(out.output_truncated, "{mode}: {out:?}");
            assert!(!out.success());
            assert!(out.stdout.len() <= MAX_CAPTURE_BYTES);
        }
    }

    async fn descendant_pid(dir: &Path) -> u32 {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if let Ok(text) = std::fs::read_to_string(dir.join("descendant.pid")) {
                if let Ok(pid) = text.parse() {
                    return pid;
                }
            }
            assert!(Instant::now() < deadline, "fixture descendant did not initialize");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    fn process_alive(pid: u32) -> bool {
        #[cfg(windows)]
        {
            use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
            use windows_sys::Win32::System::Threading::{OpenProcess, WaitForSingleObject, PROCESS_SYNCHRONIZE};
            // SAFETY: the PID is a fixture child; the checked handle is owned once.
            unsafe {
                let raw = OpenProcess(PROCESS_SYNCHRONIZE, 0, pid);
                if raw.is_null() {
                    return false;
                }
                let handle = OwnedHandle::from_raw_handle(raw);
                WaitForSingleObject(handle.as_raw_handle(), 0) == windows_sys::Win32::Foundation::WAIT_TIMEOUT
            }
        }
        #[cfg(unix)]
        {
            // Linux zombies have terminated, although the init process may not have reaped them yet.
            #[cfg(target_os = "linux")]
            if std::fs::read_to_string(format!("/proc/{pid}/stat"))
                .ok()
                .and_then(|s| s.rsplit_once(')').map(|(_, tail)| tail.trim_start().starts_with('Z')))
                == Some(true)
            {
                return false;
            }
            // SAFETY: signal 0 only queries existence; PID came from our fixture.
            unsafe { libc::kill(pid as libc::pid_t, 0) == 0 }
        }
    }

    async fn assert_descendant_stopped(pid: u32) {
        let deadline = Instant::now() + Duration::from_secs(3);
        while process_alive(pid) && Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(!process_alive(pid), "descendant {pid} survived runner cleanup");
    }

    #[tokio::test]
    async fn deadline_remains_active_after_direct_child_exit() {
        let dir = tempfile::tempdir().unwrap();
        let out =
            run(&fixture_spec("exit-with-descendant", dir.path()), Duration::from_secs(1), &CancellationToken::new())
                .await
                .unwrap();
        assert!(out.timed_out, "{out:?}");
        assert!(out.duration < Duration::from_secs(5));
        assert_descendant_stopped(descendant_pid(dir.path()).await).await;
    }

    #[tokio::test]
    async fn inherited_pipes_have_a_bounded_post_exit_drain() {
        let dir = tempfile::tempdir().unwrap();
        let out =
            run(&fixture_spec("exit-with-descendant", dir.path()), Duration::from_secs(30), &CancellationToken::new())
                .await
                .unwrap();
        assert!(out.drain_timed_out, "{out:?}");
        assert!(!out.success());
        assert!(out.duration < Duration::from_secs(6));
        assert_descendant_stopped(descendant_pid(dir.path()).await).await;
    }

    #[tokio::test]
    async fn cancellation_and_future_drop_kill_descendants_after_parent_exit() {
        for abort in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            let spec = fixture_spec("exit-with-descendant", dir.path());
            let cancel = CancellationToken::new();
            let child_cancel = cancel.clone();
            let runner = tokio::spawn(async move { run(&spec, Duration::from_secs(30), &child_cancel).await });
            let pid = descendant_pid(dir.path()).await;
            tokio::time::sleep(Duration::from_millis(100)).await;
            if abort {
                runner.abort();
                assert!(runner.await.unwrap_err().is_cancelled());
            } else {
                cancel.cancel();
                assert!(runner.await.unwrap().unwrap().cancelled);
            }
            assert_descendant_stopped(pid).await;
        }
    }

    #[tokio::test]
    async fn blocking_bridge_works_inside_tokio() {
        let out = run_blocking(&echo_spec("bridge"), Duration::from_secs(10), &CancellationToken::new()).unwrap();
        assert!(out.success());
        assert!(out.stdout.contains("bridge"));
    }
}
