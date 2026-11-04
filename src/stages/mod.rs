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
        prompt: String,
        mode: Mode,
        schema: Option<serde_json::Value>,
    ) -> AgentTask {
        let attempt = self.state.attempt_count(stage).max(1);
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
}
