//! Run directory layout: `~/.lyra/runs/<run-id>/{run.json,events.jsonl,worktree/,artifacts/,attempts/}`.

use crate::events::StageId;
use anyhow::{Context, Result};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone)]
pub struct RunDir {
    pub root: PathBuf,
}

impl RunDir {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    pub fn create(&self) -> Result<()> {
        for d in [self.artifacts(), self.transcripts(), self.attempts()] {
            std::fs::create_dir_all(&d).with_context(|| format!("creating {}", d.display()))?;
        }
        for s in StageId::ALL {
            let d = s.artifact_dir();
            if !d.is_empty() {
                std::fs::create_dir_all(self.artifacts().join(d))?;
            }
        }
        Ok(())
    }

    pub fn worktree(&self) -> PathBuf {
        self.root.join("worktree")
    }
    pub fn artifacts(&self) -> PathBuf {
        self.root.join("artifacts")
    }
    pub fn transcripts(&self) -> PathBuf {
        self.artifacts().join("transcripts")
    }
    pub fn attempts(&self) -> PathBuf {
        self.root.join("attempts")
    }
    pub fn stage_dir(&self, stage: StageId) -> PathBuf {
        let d = stage.artifact_dir();
        if d.is_empty() {
            self.artifacts()
        } else {
            self.artifacts().join(d)
        }
    }

    /// Writes a file under `artifacts/` and returns its path relative to `artifacts/`.
    pub fn write(&self, rel: impl AsRef<Path>, contents: impl AsRef<[u8]>) -> Result<String> {
        let rel = rel.as_ref();
        let path = self.artifacts().join(rel);
        if let Some(p) = path.parent() {
            std::fs::create_dir_all(p)?;
        }
        std::fs::write(&path, contents).with_context(|| format!("writing {}", path.display()))?;
        Ok(rel.to_string_lossy().replace('\\', "/"))
    }

    pub fn write_json<T: serde::Serialize>(&self, rel: impl AsRef<Path>, value: &T) -> Result<String> {
        self.write(rel, serde_json::to_string_pretty(value)?)
    }

    pub fn read(&self, rel: impl AsRef<Path>) -> Result<String> {
        let path = self.artifacts().join(rel.as_ref());
        std::fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))
    }

    pub fn read_json<T: serde::de::DeserializeOwned>(&self, rel: impl AsRef<Path>) -> Result<T> {
        Ok(serde_json::from_str(&self.read(rel)?)?)
    }

    pub fn exists(&self, rel: impl AsRef<Path>) -> bool {
        self.artifacts().join(rel.as_ref()).exists()
    }

    /// Moves the artifact directories of stages >= `from` into `attempts/<n>/` and recreates them empty.
    pub fn archive_from(&self, from: StageId, attempt_n: u32) -> Result<()> {
        let dest = self.attempts().join(format!("{attempt_n:03}-{}", from.as_str()));
        std::fs::create_dir_all(&dest)?;
        for s in StageId::ALL.iter().copied().filter(|s| *s >= from) {
            let d = s.artifact_dir();
            if d.is_empty() {
                continue;
            }
            let src = self.artifacts().join(d);
            if src.exists() {
                let target = dest.join(d);
                if target.exists() {
                    std::fs::remove_dir_all(&target)?;
                }
                std::fs::rename(&src, &target)?;
            }
            std::fs::create_dir_all(&src)?;
        }
        Ok(())
    }

    /// Lists artifact files (relative paths, forward slashes), sorted.
    pub fn list(&self) -> Vec<String> {
        let mut out = Vec::new();
        fn walk(base: &Path, dir: &Path, out: &mut Vec<String>) {
            if let Ok(rd) = std::fs::read_dir(dir) {
                for e in rd.flatten() {
                    let p = e.path();
                    if p.is_dir() {
                        walk(base, &p, out);
                    } else if let Ok(rel) = p.strip_prefix(base) {
                        out.push(rel.to_string_lossy().replace('\\', "/"));
                    }
                }
            }
        }
        walk(&self.artifacts(), &self.artifacts(), &mut out);
        out.sort();
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layout_and_archive() {
        let dir = tempfile::tempdir().unwrap();
        let rd = RunDir::new(dir.path());
        rd.create().unwrap();
        assert!(rd.stage_dir(StageId::Repro).exists());
        rd.write("20-repro/output.txt", "boom").unwrap();
        rd.write("40-fix/fix.diff", "diff").unwrap();
        rd.write("00-issue.md", "issue").unwrap();
        assert_eq!(rd.list(), vec!["00-issue.md", "20-repro/output.txt", "40-fix/fix.diff"]);
        rd.archive_from(StageId::Fix, 1).unwrap();
        assert!(rd.exists("20-repro/output.txt"));
        assert!(!rd.exists("40-fix/fix.diff"));
        assert!(rd.attempts().join("001-fix/40-fix/fix.diff").exists());
        assert!(rd.stage_dir(StageId::Fix).exists());
    }
}
