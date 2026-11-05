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
use crate::error::{ErrorKind, StageError, StageOutcome};
use crate::events::{EventSink, StageId};
use crate::profile::ExecutionProfile;
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

    /// The execution profile this run was resolved with (PLAN §13). Fails visibly when the
    /// saved run names a profile this binary does not provide.
    pub fn profile(&self) -> Result<Box<dyn ExecutionProfile>, StageError> {
        self.state.profile().map_err(|reason| StageError::fatal(ErrorKind::Config, reason))
    }

    /// Builds an `AgentTask` for this run with the standard paths filled in.
    pub fn agent_task(
        &self,
        stage: StageId,
        mut prompt: String,
        mode: Mode,
        schema: Option<serde_json::Value>,
    ) -> Result<AgentTask, StageError> {
        let attempt = self.state.attempt_count(stage).max(1);
        // Codex command tools are disabled in both modes. Supply source evidence
        // for apply_patch as well as analysis; Claude can also use native file tools.
        let profile = self.profile()?;
        prompt.push_str(&readonly_source_snapshot(&self.run_dir.worktree(), &self.relevant_source_paths(), &*profile));
        Ok(AgentTask {
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
        })
    }

    fn relevant_source_paths(&self) -> Vec<String> {
        let mut paths = Vec::new();
        for (artifact, array, field) in
            [("10-context/summary.json", "candidate_files", "path"), ("30-rca/rca.json", "culprits", "file")]
        {
            if let Ok(value) = self.run_dir.read_json::<serde_json::Value>(artifact) {
                if let Some(entries) = value.get(array).and_then(serde_json::Value::as_array) {
                    paths.extend(entries.iter().filter_map(|entry| entry.get(field)?.as_str().map(str::to_owned)));
                }
            }
        }
        if let Some(repro) = &self.state.frozen_repro {
            paths.push(repro.test_file.clone());
            paths.extend(repro.registration_file.iter().cloned());
        }
        paths
    }

    /// Renders a prompt template from `src/prompts/` with the given values.
    pub fn render(&self, template_name: &str, template: &str, values: serde_json::Value) -> anyhow::Result<String> {
        let mut env = minijinja::Environment::new();
        env.add_template(template_name, template)?;
        let tmpl = env.get_template(template_name)?;
        Ok(tmpl.render(values)?)
    }
}

/// Line-numbered source evidence for the agent: the relevant files completely (with the
/// profile's ancestor manifests), then a bounded overview of the profile's source files.
fn readonly_source_snapshot(root: &std::path::Path, relevant: &[String], profile: &dyn ExecutionProfile) -> String {
    use std::io::Read;
    const FILE_LIMIT: usize = 24_000;
    const TOTAL_LIMIT: usize = 240_000;
    // Artifact paths are model output. Check their syntax and resolved location
    // before reading them or deriving ancestor manifests.
    fn safe_source(root: &std::path::Path, raw: &str) -> Option<std::path::PathBuf> {
        use std::path::{Component, Path};
        let normalized = raw.replace('\\', "/");
        let relative = Path::new(&normalized);
        if normalized.is_empty()
            || normalized.contains(':')
            || relative.components().any(|part| !matches!(part, Component::Normal(name) if name != ".git"))
        {
            return None;
        }
        let resolved = dunce::canonicalize(root.join(relative)).ok()?;
        (resolved.starts_with(root) && resolved.is_file()).then_some(relative.to_path_buf())
    }
    fn append_source(snapshot: &mut String, relative: &std::path::Path, source: &str) {
        snapshot.push_str(&format!("\nFile: {}\n", relative.display()));
        for (index, line) in source.lines().enumerate() {
            snapshot.push_str(&format!("{}: {}\n", index + 1, line));
        }
    }
    fn collect(
        dir: &std::path::Path,
        depth: usize,
        paths: &mut Vec<std::path::PathBuf>,
        profile: &dyn ExecutionProfile,
    ) {
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
                let name = name.to_string_lossy();
                if !name.starts_with('.') && !profile.generated_dirs().contains(&name.as_ref()) {
                    collect(&path, depth + 1, paths, profile);
                }
            } else if kind.is_file() && profile.is_source_file(&path) {
                paths.push(path);
            }
            if paths.len() >= 2000 {
                break;
            }
        }
    }
    let mut paths = Vec::new();
    collect(root, 0, &mut paths, profile);
    paths.sort();
    let mut snapshot = String::new();
    let canonical_root = dunce::canonicalize(root).ok();
    let mut complete = std::collections::BTreeSet::new();
    if let Some(canonical_root) = canonical_root.as_deref() {
        for raw in relevant {
            match safe_source(canonical_root, raw) {
                Some(path) => {
                    complete.insert(path.clone());
                    for ancestor in path.parent().into_iter().flat_map(std::path::Path::ancestors) {
                        for name in profile.manifest_names() {
                            let manifest = ancestor.join(name);
                            if let Some(manifest) = safe_source(canonical_root, &manifest.to_string_lossy()) {
                                complete.insert(manifest);
                            }
                        }
                    }
                }
                None => snapshot
                    .push_str(&format!("\n[Relevant source omitted: invalid, missing or unsafe path {raw:?}.]\n")),
            }
        }
        for path in &complete {
            match std::fs::read_to_string(canonical_root.join(path)) {
                Ok(source) => append_source(&mut snapshot, path, &source),
                Err(error) => {
                    snapshot.push_str(&format!("\n[Relevant source omitted: {}: {error}.]\n", path.display()))
                }
            }
        }
    }
    let mut overview_bytes = 0;
    snapshot.push_str("\n[Fallback source overview: bounded by file count, directory depth and bytes; other sources may be omitted.]\n");
    for path in paths {
        let relative = path.strip_prefix(root).unwrap_or(&path);
        if complete.contains(relative) {
            continue;
        }
        if overview_bytes >= TOTAL_LIMIT {
            snapshot.push_str("\n[Remaining source files omitted: snapshot limit reached.]\n");
            break;
        }
        // Also validate fallback files, including ancestors replaced by symlinks.
        if canonical_root.as_deref().is_none_or(|root| safe_source(root, &relative.to_string_lossy()).is_none()) {
            continue;
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
        let start = snapshot.len();
        append_source(&mut snapshot, relative, &String::from_utf8_lossy(&bytes));
        if truncated {
            snapshot.push_str("[File truncated.]\n");
        }
        overview_bytes += snapshot.len() - start;
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
        let snapshot = readonly_source_snapshot(dir.path(), &[], &crate::profile::rust_cargo::RustCargo);
        assert!(snapshot.contains("1: pub fn culprit() {}"));
        assert!(!snapshot.contains("DO_NOT_INCLUDE"));
        assert!(snapshot.contains("untrusted_repository_source"));
    }

    #[test]
    fn edit_task_receives_source_without_a_shell() {
        let dir = tempfile::tempdir().unwrap();
        let rd = RunDir::new(dir.path().join("run"));
        rd.create().unwrap();
        std::fs::create_dir_all(rd.worktree().join("src")).unwrap();
        std::fs::write(rd.worktree().join("src/lib.rs"), "pub fn edit_me() {}\n").unwrap();
        std::fs::write(rd.worktree().join("Cargo.toml"), "[workspace]\n").unwrap();
        std::fs::write(
            rd.worktree().join("src/zz_culprit.rs"),
            format!("{}\nfn complete_tail() {{}}\n", "// source\n".repeat(4000)),
        )
        .unwrap();
        std::fs::write(dir.path().join("secret.rs"), "OUTSIDE_SECRET_MUST_NOT_BE_READ").unwrap();
        rd.write_json(
            "30-rca/rca.json",
            &serde_json::json!({
                "culprits": [{"file": "src/zz_culprit.rs"}, {"file": "../../secret.rs"},
                    {"file": dir.path().join("secret.rs").to_string_lossy()}]
            }),
        )
        .unwrap();
        let request = crate::state::RunRequest {
            repo: rd.worktree(),
            issue: "1".into(),
            issue_text: None,
            provider: None,
            policy_acknowledged: false,
        };
        let ctx = StageCtx {
            events: EventSink::open("fixture", &dir.path().join("events.jsonl")).unwrap(),
            run_dir: rd,
            state: RunState::new("fixture".into(), request, "codex".into()),
            config: Config::default(),
            agent: crate::providers::make_agent("codex", &crate::config::ProviderConfig::default()),
            cancel: CancellationToken::new(),
        };
        let task = ctx.agent_task(StageId::Fix, "Fix the issue".into(), Mode::Edit, None).unwrap();
        assert!(task.prompt.contains("1: pub fn edit_me() {}"));
        assert!(
            task.prompt.contains("4002: fn complete_tail() {}"),
            "the complete culprit exceeds the overview's per-file limit"
        );
        assert!(task.prompt.contains("1: [workspace]"));
        assert!(!task.prompt.contains("OUTSIDE_SECRET_MUST_NOT_BE_READ"));
        assert!(task.prompt.contains("Relevant source omitted: invalid, missing or unsafe path"));
        assert!(task.prompt.find("fn complete_tail()").unwrap() < task.prompt.find("pub fn edit_me()").unwrap());
        assert_eq!(task.mode, Mode::Edit);
    }

    #[cfg(unix)]
    #[test]
    fn relevant_source_rejects_symlink_escape() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("repo");
        std::fs::create_dir(&root).unwrap();
        std::fs::write(dir.path().join("secret.rs"), "SYMLINK_OUTSIDE_SECRET").unwrap();
        std::os::unix::fs::symlink(dir.path().join("secret.rs"), root.join("linked.rs")).unwrap();
        let snapshot = readonly_source_snapshot(&root, &["linked.rs".into()], &crate::profile::rust_cargo::RustCargo);
        assert!(!snapshot.contains("SYMLINK_OUTSIDE_SECRET"));
        assert!(snapshot.contains("Relevant source omitted"));
    }
}
