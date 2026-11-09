use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use lyra::config::{lyra_home, Config};
use lyra::engine::Engine;
use lyra::events::{AgentEvent, Event, EventKind, StageId};
use lyra::state::{RunRequest, Status};
use std::path::PathBuf;

#[derive(Parser)]
#[command(name = "lyra", version, about = "Issue -> draft PR harness around coding-agent CLIs")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Run the pipeline for one issue from the terminal (the dashboard does the same through the API).
    Run {
        /// Path of an already-cloned repository.
        #[arg(long)]
        repo: PathBuf,
        /// Issue URL, `#123` or `123`.
        #[arg(long)]
        issue: String,
        /// Read issue text from a file instead of fetching it from GitHub.
        #[arg(long)]
        issue_file: Option<PathBuf>,
        /// Provider id from config (`claude` | `codex`).
        #[arg(long)]
        provider: Option<String>,
        /// Acknowledge the repository's contribution policy (required to push when one exists).
        #[arg(long)]
        ack_policy: bool,
        /// Execution profile for a repository that matches more than one (`rust-cargo` | `python-pytest`).
        /// Default: the first match in detection order.
        #[arg(long)]
        profile: Option<String>,
    },
    /// Retry a finished run from a stage (intake|context|repro|rca|fix|verify|pr).
    Retry {
        run_id: String,
        #[arg(long)]
        from: String,
        /// Acknowledge the contribution policy discovered by the previous attempt.
        #[arg(long)]
        ack_policy: bool,
    },
    /// Start the dashboard (the main entry point): http://127.0.0.1:7700 by default.
    Serve {
        /// Override `[server].bind`.
        #[arg(long)]
        bind: Option<String>,
        /// Do not open the browser.
        #[arg(long)]
        no_open: bool,
    },
    /// List runs.
    Runs,
    /// Check runtimes, authentication and tools.
    Doctor,
    /// Print the effective configuration.
    Config,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_target(false)
        .init();
    let cli = Cli::parse();
    let home = lyra_home();
    Config::ensure_default(&home)?;
    let config = Config::load(&home)?;

    match cli.cmd {
        Cmd::Config => {
            println!("{}", toml::to_string_pretty(&config)?);
        }
        Cmd::Doctor => {
            let report = lyra::doctor::run(&config).await;
            println!("{}", report.render_text());
            if !report.ok() {
                std::process::exit(1);
            }
        }
        Cmd::Serve { bind, no_open } => {
            let bind = bind.unwrap_or_else(|| config.server.bind.clone());
            let open_browser = config.server.open_browser && !no_open;
            let engine = Engine::new(home, config);
            lyra::web::serve(engine, &bind, open_browser).await?;
        }
        Cmd::Runs => {
            let engine = Engine::new(home, config);
            for st in engine.list() {
                println!(
                    "{:<48} {:<12} {:<8} {}",
                    st.id,
                    st.status.as_str(),
                    st.stage,
                    st.pr_url.clone().unwrap_or_default()
                );
            }
        }
        Cmd::Run { repo, issue, issue_file, provider, ack_policy, profile } => {
            let issue_text = match issue_file {
                Some(p) => Some(std::fs::read_to_string(&p).with_context(|| format!("reading {}", p.display()))?),
                None => None,
            };
            let engine = Engine::new(home, config);
            engine.mark_interrupted()?;
            let req = RunRequest {
                repo: dunce::canonicalize(&repo).with_context(|| format!("opening {}", repo.display()))?,
                issue,
                issue_text,
                provider,
                policy_acknowledged: ack_policy,
                profile,
            };
            let id = engine.submit(req)?;
            println!("run {id}");
            let code = follow(&engine, &id).await;
            std::process::exit(code);
        }
        Cmd::Retry { run_id, from, ack_policy } => {
            let stage = StageId::parse(&from).with_context(|| format!("unknown stage '{from}'"))?;
            let engine = Engine::new(home, config);
            engine.mark_interrupted()?;
            engine.retry_with_policy_ack(&run_id, stage, ack_policy)?;
            let code = follow(&engine, &run_id).await;
            std::process::exit(code);
        }
    }
    Ok(())
}

/// Prints events as they happen until the run reaches a terminal status. Returns an exit code.
async fn follow(engine: &std::sync::Arc<Engine>, id: &str) -> i32 {
    let rx = match engine.subscribe(id) {
        Some(rx) => rx,
        None => return 2,
    };
    follow_events(
        rx,
        |since| engine.history(id, since),
        || {
            if engine.is_active(id) {
                (true, None)
            } else {
                (false, engine.get(id).ok().map(|state| state.status))
            }
        },
        || {
            engine.cancel(id);
        },
        |event| print_event(&event.kind),
    )
    .await
}

async fn follow_events(
    mut rx: tokio::sync::broadcast::Receiver<Event>,
    mut history: impl FnMut(u64) -> Vec<Event>,
    mut run_status: impl FnMut() -> (bool, Option<Status>),
    mut cancel: impl FnMut(),
    mut on_event: impl FnMut(&Event),
) -> i32 {
    let mut seq = 0;
    let mut stopping = false;
    let mut terminal = None;
    let mut replay = true;
    let mut stream_closed = false;
    loop {
        // Subscribe before replay so even a run that finished before follow() is observable.
        // Subsequent events arrive through broadcast; reread the durable log only on lag.
        if replay {
            for ev in history(seq) {
                if ev.seq > seq {
                    seq = ev.seq;
                    on_event(&ev);
                    terminal = event_exit_code(&ev.kind).or(terminal);
                }
            }
            replay = false;
        }
        let (active, status) = run_status();
        if !active {
            let code = status.filter(|status| status.is_terminal()).map(status_code).or(terminal);
            if let Some(code) = code {
                // Admission is released only after the driver's final event is emitted.
                // Drain that durable tail once even when terminal state beats queued live events.
                for ev in history(seq) {
                    if ev.seq > seq {
                        seq = ev.seq;
                        on_event(&ev);
                    }
                }
                return code;
            }
        }
        tokio::select! {
            _ = tokio::signal::ctrl_c(), if !stopping => { stopping = true; cancel(); }
            ev = rx.recv(), if !stream_closed => {
                match ev {
                    Ok(ev) if ev.seq > seq => {
                        seq = ev.seq;
                        on_event(&ev);
                        terminal = event_exit_code(&ev.kind).or(terminal);
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => replay = true,
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => stream_closed = true,
                    _ => {}
                }
            }
            _ = tokio::time::sleep(std::time::Duration::from_millis(50)) => {}
        }
    }
}

fn event_exit_code(kind: &EventKind) -> Option<i32> {
    match kind {
        EventKind::RunFinished { .. } => Some(0),
        EventKind::RunStopped { .. } => Some(3),
        EventKind::StageFailed { .. } => Some(4),
        EventKind::RunCancelled { .. } | EventKind::RunInterrupted { .. } => Some(5),
        _ => None,
    }
}

fn status_code(status: Status) -> i32 {
    match status {
        Status::Done => 0,
        Status::Stopped => 3,
        Status::Failed => 4,
        Status::Cancelled | Status::Interrupted => 5,
        _ => 1,
    }
}

fn print_event(kind: &EventKind) {
    match kind {
        EventKind::StageStarted { stage, attempt } => println!("==> {} (attempt {attempt})", stage.label()),
        EventKind::StageCompleted { stage, duration_ms, .. } => {
            println!("    {} done in {:.1}s", stage.label(), *duration_ms as f64 / 1000.0)
        }
        EventKind::StageFailed { stage, error, .. } => println!("    {} FAILED: {error}", stage.label()),
        EventKind::RunStopped { stage, reason, message } => {
            println!("STOPPED at {}: {reason} — {message}", stage.label())
        }
        EventKind::RunFinished { pr_url } => println!("DONE {}", pr_url.clone().unwrap_or_default()),
        EventKind::RunCancelled { .. } => println!("CANCELLED"),
        EventKind::Agent { event, .. } => match event {
            AgentEvent::ToolCall { name, summary } => println!("    [{name}] {summary}"),
            AgentEvent::Text { text } => println!("    {}", text.lines().next().unwrap_or_default()),
            AgentEvent::FileChanged { path } => println!("    [edit] {path}"),
            AgentEvent::Error { message } => println!("    [error] {message}"),
            _ => {}
        },
        EventKind::Log { level, message } => println!("    {level:?}: {message}"),
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stream_event(seq: u64, terminal: bool) -> Event {
        Event {
            seq,
            ts: String::new(),
            run_id: "fixture".into(),
            kind: if terminal {
                EventKind::RunFinished { pr_url: None }
            } else {
                EventKind::Log { level: lyra::events::Level::Info, message: String::new() }
            },
        }
    }

    #[tokio::test]
    async fn live_follow_replays_once_instead_of_rereading_per_event() {
        let (sender, receiver) = tokio::sync::broadcast::channel(256);
        for seq in 1..=100 {
            sender.send(stream_event(seq, seq == 100)).unwrap();
        }
        let mut replays = 0;
        let code = follow_events(
            receiver,
            |_| {
                replays += 1;
                Vec::new()
            },
            || (false, None),
            || panic!("unexpected cancel"),
            |_| {},
        )
        .await;
        assert_eq!(code, 0);
        assert_eq!(replays, 2, "only initial and final replay should read the event log");
    }

    #[tokio::test]
    async fn lagged_follow_replays_missing_events_from_last_seen_sequence() {
        let (sender, receiver) = tokio::sync::broadcast::channel(2);
        for seq in 1..=10 {
            sender.send(stream_event(seq, seq == 10)).unwrap();
        }
        let mut cursors = Vec::new();
        let code = follow_events(
            receiver,
            |since| {
                cursors.push(since);
                if cursors.len() == 1 {
                    vec![stream_event(1, false)]
                } else {
                    (2..=10).map(|seq| stream_event(seq, seq == 10)).collect()
                }
            },
            || (false, None),
            || panic!("unexpected cancel"),
            |_| {},
        )
        .await;
        assert_eq!(code, 0);
        assert_eq!(cursors, [0, 1, 10]);
    }

    #[tokio::test]
    async fn finished_follow_processes_queued_unseen_logs_before_returning() {
        let (sender, receiver) = tokio::sync::broadcast::channel(8);
        for seq in 2..=6 {
            sender.send(stream_event(seq, seq == 6)).unwrap();
        }
        let mut cursors = Vec::new();
        let mut processed = Vec::new();
        let code = follow_events(
            receiver,
            |since| {
                cursors.push(since);
                if since == 0 {
                    vec![stream_event(1, false)]
                } else {
                    (2..=6).map(|seq| stream_event(seq, seq == 6)).collect()
                }
            },
            || (false, Some(Status::Done)),
            || panic!("unexpected cancel"),
            |event| processed.push(event.seq),
        )
        .await;
        assert_eq!(code, 0);
        assert_eq!(cursors, [0, 1]);
        assert_eq!(
            processed,
            [1, 2, 3, 4, 5, 6],
            "final replay must deliver queued logs and terminal event exactly once"
        );
    }

    #[test]
    fn retry_can_acknowledge_a_newly_discovered_policy() {
        let explicit = Cli::try_parse_from(["lyra", "retry", "run-1", "--from", "pr", "--ack-policy"]).unwrap();
        assert!(matches!(explicit.cmd, Cmd::Retry { ack_policy: true, .. }));
        let unchanged = Cli::try_parse_from(["lyra", "retry", "run-1", "--from", "pr"]).unwrap();
        assert!(matches!(unchanged.cmd, Cmd::Retry { ack_policy: false, .. }));
    }

    #[tokio::test]
    async fn follow_replays_already_finished_run() {
        let home = tempfile::tempdir().unwrap();
        let engine = Engine::new(home.path().into(), Config::default());
        let rd = engine.run_dir("finished").unwrap();
        rd.create().unwrap();
        let req = RunRequest {
            repo: PathBuf::from("."),
            issue: "1".into(),
            issue_text: None,
            provider: None,
            policy_acknowledged: false,
            profile: None,
        };
        let mut st = lyra::state::RunState::new("finished".into(), req, "claude".into());
        st.status = Status::Failed;
        st.save(&rd.root).unwrap();
        assert_eq!(
            tokio::time::timeout(std::time::Duration::from_secs(1), follow(&engine, "finished")).await.unwrap(),
            4
        );
    }
}
