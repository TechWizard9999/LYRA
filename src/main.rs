use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use lyra::config::{lyra_home, Config};
use lyra::engine::Engine;
use lyra::events::{AgentEvent, EventKind, StageId};
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
        /// Read the issue text from a file instead of GitHub (offline runs).
        #[arg(long)]
        issue_file: Option<PathBuf>,
        /// Provider id from config (`claude` | `codex`).
        #[arg(long)]
        provider: Option<String>,
        /// Acknowledge the repository's contribution policy (required to push when one exists).
        #[arg(long)]
        ack_policy: bool,
    },
    /// Retry a finished run from a stage (intake|context|repro|rca|fix|verify|pr).
    Retry {
        run_id: String,
        #[arg(long)]
        from: String,
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
        Cmd::Run { repo, issue, issue_file, provider, ack_policy } => {
            let issue_text = match issue_file {
                Some(p) => Some(std::fs::read_to_string(&p).with_context(|| format!("reading {}", p.display()))?),
                None => None,
            };
            let engine = Engine::new(home, config);
            engine.mark_interrupted();
            let req = RunRequest {
                repo: repo.canonicalize().unwrap_or(repo),
                issue,
                issue_text,
                provider,
                policy_acknowledged: ack_policy,
            };
            let id = engine.submit(req)?;
            println!("run {id}");
            let code = follow(&engine, &id).await;
            std::process::exit(code);
        }
        Cmd::Retry { run_id, from } => {
            let stage = StageId::parse(&from).with_context(|| format!("unknown stage '{from}'"))?;
            let engine = Engine::new(home, config);
            engine.mark_interrupted();
            engine.retry(&run_id, stage)?;
            let code = follow(&engine, &run_id).await;
            std::process::exit(code);
        }
    }
    Ok(())
}

/// Prints events as they happen until the run reaches a terminal status. Returns an exit code.
async fn follow(engine: &std::sync::Arc<Engine>, id: &str) -> i32 {
    let mut rx = match engine.subscribe(id) {
        Some(rx) => rx,
        None => return 2,
    };
    loop {
        match rx.recv().await {
            Ok(ev) => {
                print_event(&ev.kind);
                match ev.kind {
                    EventKind::RunFinished { .. } => return 0,
                    EventKind::RunStopped { .. } => return 3,
                    EventKind::StageFailed { .. } => return 4,
                    EventKind::RunCancelled { .. } | EventKind::RunInterrupted { .. } => return 5,
                    _ => {}
                }
            }
            Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
            Err(_) => break,
        }
    }
    match engine.get(id).map(|s| s.status) {
        Ok(Status::Done) => 0,
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
