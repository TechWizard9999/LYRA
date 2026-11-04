//! The seven stages. Each is a function of (run dir, state, config, agent) and returns a
//! `StageOutcome`; the engine owns sequencing, retries and persistence.

pub mod context;
pub mod fix;
pub mod intake;
pub mod pr;
pub mod rca;
pub mod repro;
pub mod verify;

use crate::artifacts::RunDir;
use crate::config::Config;
use crate::error::StageOutcome;
use crate::events::{EventSink, StageId};
use crate::providers::{AgentTask, Mode, SharedAgent};
use crate::state::RunState;
use async_trait::async_trait;
use std::time::Duration;
use tokio_util::sync::CancellationToken;

pub struct StageCtx {
    pub run_dir: RunDir,
    /// Mutable: stages record facts (base commit, checkpoints, verified commit, PR URL).
    /// The engine saves it after every stage.
    pub state: RunState,
    pub config: Config,
    pub agent: SharedAgent,
    pub events: EventSink,
    pub cancel: CancellationToken,
}

impl StageCtx {
    pub fn timeout(&self) -> Duration {
        self.config.timeout()
    }

    /// Builds an `AgentTask` for this run with the standard paths filled in.
    pub fn agent_task(
        &self,
        stage: StageId,
        mut prompt: String,
        mode: Mode,
        schema: Option<serde_json::Value>,
    ) -> AgentTask {
        let attempt = self.state.attempt_count(stage).max(1);
        if mode == Mode::ReadOnly {
            // Codex read-only calls have command tools disabled. Supply bounded source
            // evidence directly; Claude may additionally inspect with its native file tools.
            prompt.push_str(&readonly_source_snapshot(&self.run_dir.worktree()));
        }
        AgentTask {
            stage,
            cwd: self.run_dir.worktree(),
            artifacts: self.run_dir.artifacts(),
            prompt,
            mode,
            schema,
            transcript: self.run_dir.transcripts().join(format!(
                "{}-{}-{attempt}.jsonl",
                stage.as_str(),
                self.agent.id()
            )),
            timeout: self.timeout(),
            cancel: self.cancel.clone(),
        }
    }

    /// Renders a prompt template from `src/prompts/` with the given values.
    pub fn render(&self, template_name: &str, template: &str, values: serde_json::Value) -> anyhow::Result<String> {
        let mut env = minijinja::Environment::new();
        env.add_template(template_name, template)?;
        let tmpl = env.get_template(template_name)?;
        Ok(tmpl.render(values)?)
    }
}

fn readonly_source_snapshot(root: &std::path::Path) -> String {
    use std::io::Read;
    const FILE_LIMIT: usize = 24_000;
    const TOTAL_LIMIT: usize = 240_000;
    fn collect(dir: &std::path::Path, depth: usize, paths: &mut Vec<std::path::PathBuf>) {
        if depth > 16 || paths.len() >= 2000 {
            return;
        }
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let Ok(kind) = entry.file_type() else {
                continue;
            };
            if kind.is_symlink() {
                continue;
            }
            let path = entry.path();
            if kind.is_dir() {
                let name = entry.file_name();
                if !name.to_string_lossy().starts_with('.') && name != "target" && name != "node_modules" {
                    collect(&path, depth + 1, paths);
                }
            } else if kind.is_file()
                && (path.extension().is_some_and(|s| s == "rs") || entry.file_name() == "Cargo.toml")
            {
                paths.push(path);
            }
            if paths.len() >= 2000 {
                break;
            }
        }
    }
    let mut paths = Vec::new();
    collect(root, 0, &mut paths);
    paths.sort();
    let mut snapshot = String::new();
    for path in paths {
        if snapshot.len() >= TOTAL_LIMIT {
            snapshot.push_str("\n[Remaining source files omitted: snapshot limit reached.]\n");
            break;
        }
        let Ok(file) = std::fs::File::open(&path) else {
            continue;
        };
        let mut bytes = Vec::new();
        if file.take((FILE_LIMIT + 1) as u64).read_to_end(&mut bytes).is_err() {
            continue;
        }
        let truncated = bytes.len() > FILE_LIMIT;
        bytes.truncate(FILE_LIMIT);
        let rel = path.strip_prefix(root).unwrap_or(&path).to_string_lossy();
        snapshot.push_str(&format!("\nFile: {rel}\n"));
        for (index, line) in String::from_utf8_lossy(&bytes).lines().enumerate() {
            snapshot.push_str(&format!("{}: {}\n", index + 1, line));
        }
        if truncated {
            snapshot.push_str("[File truncated.]\n");
        }
    }
    include_str!("../prompts/readonly-source.md").replace("{{ source }}", &snapshot)
}

#[async_trait]
pub trait Stage: Send + Sync {
    fn id(&self) -> StageId;
    async fn run(&self, ctx: &mut StageCtx) -> StageOutcome;
}

/// The fixed pipeline, in order.
pub fn pipeline() -> Vec<Box<dyn Stage>> {
    vec![
        Box::new(intake::IntakeStage),
        Box::new(context::ContextStage),
        Box::new(repro::ReproStage),
        Box::new(rca::RcaStage),
        Box::new(fix::FixStage),
        Box::new(verify::VerifyStage),
        Box::new(pr::PrStage),
    ]
}

/// Embedded prompt templates and schemas.
pub mod assets {
    pub const CONTEXT_PROMPT: &str = include_str!("../prompts/context.md");
    pub const REPRO_PROMPT: &str = include_str!("../prompts/repro.md");
    pub const RCA_PROMPT: &str = include_str!("../prompts/rca.md");
    pub const FIX_PROMPT: &str = include_str!("../prompts/fix.md");
    pub const PR_BODY_TEMPLATE: &str = include_str!("../prompts/pr_body.md");
    pub const CONTEXT_SCHEMA: &str = include_str!("../schemas/context.schema.json");
    pub const REPRO_SCHEMA: &str = include_str!("../schemas/repro.schema.json");
    pub const RCA_SCHEMA: &str = include_str!("../schemas/rca.schema.json");
    pub const FIX_SCHEMA: &str = include_str!("../schemas/fix.schema.json");

    pub fn schema(text: &str) -> serde_json::Value {
        serde_json::from_str(text).expect("embedded schema is valid JSON")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pipeline_matches_stage_order() {
        let ids: Vec<StageId> = pipeline().iter().map(|s| s.id()).collect();
        assert_eq!(ids, StageId::ALL.to_vec());
    }

    #[test]
    fn embedded_schemas_parse() {
        for s in [assets::CONTEXT_SCHEMA, assets::REPRO_SCHEMA, assets::RCA_SCHEMA, assets::FIX_SCHEMA] {
            let v = assets::schema(s);
            assert_eq!(v["type"], "object");
        }
    }

    #[test]
    fn readonly_snapshot_supplies_numbered_source_without_generated_files() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("src")).unwrap();
        std::fs::create_dir(dir.path().join("target")).unwrap();
        std::fs::write(dir.path().join("src/lib.rs"), "pub fn culprit() {}\n").unwrap();
        std::fs::write(dir.path().join("target/generated.rs"), "DO_NOT_INCLUDE").unwrap();
        let snapshot = readonly_source_snapshot(dir.path());
        assert!(snapshot.contains("1: pub fn culprit() {}"));
        assert!(!snapshot.contains("DO_NOT_INCLUDE"));
        assert!(snapshot.contains("untrusted_repository_source"));
    }
}
