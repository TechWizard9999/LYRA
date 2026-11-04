//! S3 RCA: one read-only agent call fed with the context, git history and repro output.
//! `unresolved` stops the run with the analysis saved.

use super::context::{cap, readonly_guard, readonly_snapshot, run_readonly};
use super::{assets, Stage, StageCtx};
use crate::error::{ErrorKind, StageError, StageOutcome, StopReason};
use crate::events::StageId;
use crate::git;
use async_trait::async_trait;
use serde_json::{json, Value};

pub struct RcaStage;

#[async_trait]
impl Stage for RcaStage {
    fn id(&self) -> StageId {
        StageId::Rca
    }
    async fn run(&self, ctx: &mut StageCtx) -> StageOutcome {
        match rca(ctx).await {
            Ok(o) => o,
            Err(e) => StageOutcome::failed(e),
        }
    }
}

async fn rca(ctx: &mut StageCtx) -> Result<StageOutcome, StageError> {
    let wt = ctx.run_dir.worktree();
    let read = |rel: &str| ctx.run_dir.read(rel).unwrap_or_default();
    let output = read("20-repro/output.txt");
    // Keep the end of long outputs: that is where the failure is.
    let repro_output = if output.len() > 30_000 {
        let mut start = output.len() - 30_000;
        while !output.is_char_boundary(start) {
            start += 1;
        }
        format!("… (truncated)\n{}", &output[start..])
    } else {
        output
    };
    let prompt = ctx
        .render(
            "rca.md",
            assets::RCA_PROMPT,
            json!({
                "issue": read("00-issue.md"),
                "context_summary": read("10-context/summary.json"),
                "instructions": read("10-context/instructions.md"),
                "git_history": cap(&read("10-context/git-history.md"), 20_000),
                "repro_json": read("20-repro/repro.json"),
                "repro_output": repro_output,
            }),
        )
        .map_err(|e| StageError::from_anyhow(ErrorKind::Config, e))?;
    let head_before = git::head(&wt).map_err(|e| StageError::from_anyhow(ErrorKind::Git, e))?;
    let before = readonly_snapshot(&wt)?;
    let out = run_readonly(ctx, StageId::Rca, prompt, assets::RCA_SCHEMA).await?;
    let rca = out.structured.unwrap_or(Value::Null);
    let io = |e| StageError::from_anyhow(ErrorKind::Io, e);
    ctx.events.artifact(ctx.run_dir.write_json("30-rca/rca.json", &rca).map_err(io)?);
    ctx.events.artifact(ctx.run_dir.write("30-rca/rca.md", rca_markdown(&rca)).map_err(io)?);
    readonly_guard(&wt, &head_before, &before)?;

    let mechanism = rca.get("mechanism").and_then(Value::as_str).unwrap_or_default().to_string();
    match rca.get("result").and_then(Value::as_str) {
        Some("unresolved") => Ok(StageOutcome::stopped(
            StopReason::RcaUnresolved,
            if mechanism.is_empty() { "the agent could not establish the root cause".to_string() } else { mechanism },
        )),
        Some("supported") => {
            ctx.events.info(format!("rca: {}", cap(&mechanism, 300)));
            Ok(StageOutcome::Completed)
        }
        other => {
            Err(StageError::new(ErrorKind::Schema, format!("rca.result must be supported|unresolved, got {other:?}")))
        }
    }
}

/// Human-readable form of `rca.json`.
pub fn rca_markdown(rca: &Value) -> String {
    let str_of = |k: &str| rca.get(k).and_then(Value::as_str).unwrap_or("").trim().to_string();
    let mut md = format!("# Root cause analysis\n\n**Result:** {}", str_of("result"));
    if let Some(c) = rca.get("confidence").and_then(Value::as_f64) {
        md.push_str(&format!(" · **confidence:** {c:.2}"));
    }
    md.push_str(&format!("\n\n## Mechanism\n\n{}\n\n## Culprits\n\n", str_of("mechanism")));
    let culprits = rca.get("culprits").and_then(Value::as_array).cloned().unwrap_or_default();
    if culprits.is_empty() {
        md.push_str("_none identified_\n");
    }
    for c in culprits {
        let file = c.get("file").and_then(Value::as_str).unwrap_or("?");
        let sym = c.get("symbol").and_then(Value::as_str).unwrap_or("");
        let lines = c
            .get("lines")
            .and_then(Value::as_array)
            .map(|l| l.iter().map(|x| x.to_string()).collect::<Vec<_>>().join("-"));
        md.push_str(&format!("- `{file}`"));
        if let Some(l) = lines.filter(|l| !l.is_empty()) {
            md.push_str(&format!(":{l}"));
        }
        if !sym.is_empty() {
            md.push_str(&format!(" `{sym}`"));
        }
        md.push('\n');
    }
    md.push_str("\n## Evidence\n\n");
    for e in rca.get("evidence").and_then(Value::as_array).cloned().unwrap_or_default() {
        md.push_str(&format!("- {}\n", e.as_str().map(String::from).unwrap_or_else(|| e.to_string())));
    }
    if let Some(c) =
        rca.get("introduced_by").and_then(|i| i.get("commit")).and_then(Value::as_str).filter(|c| !c.is_empty())
    {
        md.push_str(&format!("\n## Introduced by\n\n`{c}`\n"));
    }
    md.push_str(&format!("\n## Fix sketch\n\n{}\n", str_of("fix_sketch")));
    md
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn markdown_sections() {
        let v = json!({
            "result": "supported",
            "culprits": [{"file": "src/parser.rs", "symbol": "parse_header", "lines": [120, 145]}],
            "mechanism": "unwrap on None",
            "evidence": ["panicked at src/parser.rs:120"],
            "introduced_by": {"commit": "abc123"},
            "fix_sketch": "return an error",
            "confidence": 0.8
        });
        let md = rca_markdown(&v);
        assert!(md.contains("**Result:** supported · **confidence:** 0.80"));
        assert!(md.contains("- `src/parser.rs`:120-145 `parse_header`"));
        assert!(md.contains("- panicked at src/parser.rs:120"));
        assert!(md.contains("`abc123`"));
        assert!(md.contains("## Fix sketch\n\nreturn an error"));
        let md = rca_markdown(&json!({"result": "unresolved", "introduced_by": null}));
        assert!(md.contains("_none identified_"));
        assert!(!md.contains("Introduced by"));
    }

    #[test]
    fn schema_shape() {
        let s = assets::schema(assets::RCA_SCHEMA);
        let req: Vec<&str> = s["required"].as_array().unwrap().iter().filter_map(Value::as_str).collect();
        for k in ["result", "culprits", "mechanism", "evidence", "fix_sketch", "confidence"] {
            assert!(req.contains(&k), "{k}");
        }
        assert_eq!(s["properties"]["result"]["enum"], json!(["supported", "unresolved"]));
    }
}
