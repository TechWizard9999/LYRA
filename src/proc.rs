//! Subprocess plumbing: spawn with a hang-guard timeout and a cancellation token, stream
//! stdout line by line, kill the whole process tree on cancel/timeout, resolve `.cmd`
//! shims on Windows, and scrub the environment for untrusted commands.

use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, BufReader};
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
    pub duration: Duration,
}

impl CommandOutput {
    pub fn success(&self) -> bool {
        self.exit_code == Some(0) && !self.timed_out && !self.cancelled
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
    cmd
}

/// Kills the process and all of its descendants.
pub async fn kill_tree(child: &mut Child) {
    if let Some(pid) = child.id() {
        #[cfg(windows)]
        {
            let _ = Command::new("taskkill").args(["/PID", &pid.to_string(), "/T", "/F"]).output().await;
        }
        #[cfg(unix)]
        {
            // Negative pid = the process group we created with process_group(0).
            let _ = Command::new("kill").args(["-9", &format!("-{pid}")]).output().await;
        }
    }
    let _ = child.kill().await;
}

/// Runs to completion, collecting stdout/stderr.
pub async fn run(spec: &CommandSpec, timeout: Duration, cancel: &CancellationToken) -> Result<CommandOutput> {
    let mut stdout = String::new();
    let mut stderr = String::new();
    let out = run_streaming(spec, timeout, cancel, |line| match line {
        StreamLine::Stdout(l) => {
            stdout.push_str(&l);
            stdout.push('\n');
        }
        StreamLine::Stderr(l) => {
            stderr.push_str(&l);
            stderr.push('\n');
        }
    })
    .await?;
    Ok(CommandOutput { stdout, stderr, ..out })
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

    if let (Some(input), Some(mut stdin)) = (spec.stdin.clone(), child.stdin.take()) {
        tokio::spawn(async move {
            use tokio::io::AsyncWriteExt;
            let _ = stdin.write_all(input.as_bytes()).await;
            let _ = stdin.shutdown().await;
        });
    }

    let stdout = child.stdout.take().context("no stdout")?;
    let stderr = child.stderr.take().context("no stderr")?;
    let mut out_lines = BufReader::new(stdout).lines();
    let mut err_reader = BufReader::new(stderr);
    let mut err_buf = Vec::new();

    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<StreamLine>();
    let tx_err = tx.clone();
    let err_task = tokio::spawn(async move {
        let mut chunk = [0u8; 4096];
        let mut pending = String::new();
        loop {
            match err_reader.read(&mut chunk).await {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    err_buf.extend_from_slice(&chunk[..n]);
                    pending.push_str(&String::from_utf8_lossy(&chunk[..n]));
                    while let Some(i) = pending.find('\n') {
                        let line = pending[..i].trim_end_matches('\r').to_string();
                        pending = pending[i + 1..].to_string();
                        let _ = tx_err.send(StreamLine::Stderr(line));
                    }
                }
            }
        }
        if !pending.is_empty() {
            let _ = tx_err.send(StreamLine::Stderr(pending));
        }
    });
    let out_task = tokio::spawn(async move {
        while let Ok(Some(line)) = out_lines.next_line().await {
            let _ = tx.send(StreamLine::Stdout(line));
        }
    });

    let deadline = tokio::time::sleep(timeout);
    tokio::pin!(deadline);
    let mut timed_out = false;
    let mut cancelled = false;
    let mut exit_code: Option<i32> = None;
    let mut exited = false;

    loop {
        tokio::select! {
            biased;
            _ = cancel.cancelled(), if !cancelled && !exited => {
                cancelled = true;
                kill_tree(&mut child).await;
            }
            _ = &mut deadline, if !timed_out && !exited => {
                timed_out = true;
                kill_tree(&mut child).await;
            }
            status = child.wait(), if !exited => {
                exited = true;
                exit_code = status.ok().and_then(|s| s.code());
            }
            line = rx.recv() => {
                match line {
                    Some(l) => on_line(l),
                    None => break, // both pipes closed
                }
            }
        }
    }
    if !exited {
        exit_code = child.wait().await.ok().and_then(|s| s.code());
    }
    let _ = out_task.await;
    let _ = err_task.await;
    Ok(CommandOutput {
        exit_code,
        stdout: String::new(),
        stderr: String::new(),
        timed_out,
        cancelled,
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
}
