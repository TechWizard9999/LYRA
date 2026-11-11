//! Configuration: `~/.lyra/config.toml` (or `$LYRA_HOME/config.toml`) plus an optional
//! `<repo>/lyra.toml` that can override the verify commands.
//!
//! Verify commands are per execution profile ([`Config::verify_commands`]): the repository's
//! `lyra.toml [verify]` wins, then the operator's `[profiles.<id>.verify]`, then (for
//! `rust-cargo` only) the operator's historical top-level `[verify]`, then the profile's
//! built-in default.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub run: RunConfig,
    pub server: ServerConfig,
    pub providers: BTreeMap<String, ProviderConfig>,
    /// The `rust-cargo` verify commands (the table predates execution profiles).
    pub verify: VerifyConfig,
    /// Per-profile settings, `[profiles.<profile id>]`.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub profiles: BTreeMap<String, ProfileConfig>,
    /// `[verify]` from the target repository's `lyra.toml`: like the operator's `[verify]`,
    /// it predates execution profiles and applies to `rust-cargo` only, so a mixed repository
    /// never runs its Cargo commands as another profile's suite. Never read from or written
    /// to `config.toml`.
    #[serde(skip)]
    pub repository_verify: VerifyCommands,
    /// `[profiles.<id>.verify]` from the target repository's `lyra.toml`, per profile.
    #[serde(skip)]
    pub repository_profiles: BTreeMap<String, VerifyCommands>,
}

/// Verify commands for one profile; `None` means "not set at this level".
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct VerifyCommands {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub build: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub test: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fmt: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ProfileConfig {
    pub verify: VerifyCommands,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct RunConfig {
    /// Provider id: a key of `[providers]` (`claude` | `codex`).
    pub provider: String,
    /// Hang guard for one agent call or one verify command. Not a budget.
    pub timeout_minutes: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ServerConfig {
    pub bind: String,
    pub open_browser: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderKind {
    Claude,
    Codex,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ClaudeAuth {
    /// `--bare`, requires `ANTHROPIC_API_KEY`.
    ApiKey,
    /// Uses the `claude` login; project settings and MCP servers are excluded.
    Login,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ProviderConfig {
    pub kind: ProviderKind,
    pub model: Option<String>,
    pub auth: ClaudeAuth,
    /// Binary name or path; defaults to `claude` / `codex`.
    pub bin: Option<String>,
    /// Narrowly allowlisted presentation/effort arguments; execution-policy overrides are rejected.
    pub extra_args: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct VerifyConfig {
    pub build: String,
    pub test: String,
    pub fmt: String,
}

impl Default for RunConfig {
    fn default() -> Self {
        Self { provider: "claude".into(), timeout_minutes: 30 }
    }
}
impl Default for ServerConfig {
    fn default() -> Self {
        Self { bind: "127.0.0.1:7700".into(), open_browser: true }
    }
}
impl Default for ProviderConfig {
    fn default() -> Self {
        Self { kind: ProviderKind::Claude, model: None, auth: ClaudeAuth::ApiKey, bin: None, extra_args: Vec::new() }
    }
}
impl Default for VerifyConfig {
    fn default() -> Self {
        Self {
            build: "cargo build --workspace".into(),
            test: "cargo test --workspace".into(),
            fmt: "cargo fmt --all -- --check".into(),
        }
    }
}
impl Default for Config {
    fn default() -> Self {
        let mut providers = BTreeMap::new();
        providers.insert(
            "claude".to_string(),
            ProviderConfig { kind: ProviderKind::Claude, model: Some("opus".into()), ..Default::default() },
        );
        providers.insert(
            "codex".to_string(),
            ProviderConfig { kind: ProviderKind::Codex, model: None, ..Default::default() },
        );
        Self {
            run: RunConfig::default(),
            server: ServerConfig::default(),
            providers,
            verify: VerifyConfig::default(),
            profiles: BTreeMap::new(),
            repository_verify: VerifyCommands::default(),
            repository_profiles: BTreeMap::new(),
        }
    }
}

/// Only verify commands may be overridden per repository here: `[verify]` (`rust-cargo`) and
/// `[profiles.<id>.verify]`. Profile-specific keys are read by the profile itself from the same
/// file, not here: `packages` under `[profiles.python-pytest]`, and `sources` and `config` under
/// `[profiles.js-vitest]`. Unknown keys are ignored by this struct.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct RepoOverrides {
    verify: Option<VerifyCommands>,
    profiles: BTreeMap<String, RepoProfile>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct RepoProfile {
    verify: Option<VerifyCommands>,
}

/// `$LYRA_HOME` or `~/.lyra`.
pub fn lyra_home() -> PathBuf {
    if let Ok(p) = std::env::var("LYRA_HOME") {
        if !p.trim().is_empty() {
            return PathBuf::from(p);
        }
    }
    dirs::home_dir().unwrap_or_else(|| PathBuf::from(".")).join(".lyra")
}

pub fn runs_dir(home: &Path) -> PathBuf {
    home.join("runs")
}

impl Config {
    pub fn parse(toml_text: &str) -> Result<Self> {
        let cfg: Config = toml::from_str(toml_text).context("parsing config.toml")?;
        cfg.validate()?;
        Ok(cfg)
    }

    /// Loads `<home>/config.toml`, falling back to defaults when it does not exist.
    pub fn load(home: &Path) -> Result<Self> {
        let path = home.join("config.toml");
        if path.exists() {
            let text = std::fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
            Self::parse(&text)
        } else {
            Ok(Self::default())
        }
    }

    /// Writes a default config if none exists; returns its path.
    pub fn ensure_default(home: &Path) -> Result<PathBuf> {
        let path = home.join("config.toml");
        if !path.exists() {
            std::fs::create_dir_all(home)?;
            std::fs::write(&path, DEFAULT_CONFIG_TOML)?;
        }
        Ok(path)
    }

    pub fn validate(&self) -> Result<()> {
        anyhow::ensure!(
            self.providers.contains_key(&self.run.provider),
            "run.provider = \"{}\" is not defined under [providers]",
            self.run.provider
        );
        anyhow::ensure!(self.run.timeout_minutes > 0, "run.timeout_minutes must be > 0");
        Ok(())
    }

    /// Records `<repo>/lyra.toml` overrides (verify commands only) for [`Self::verify_commands`].
    pub fn with_repo_overrides(mut self, repo: &Path) -> Result<Self> {
        let path = repo.join("lyra.toml");
        if path.exists() {
            let text = std::fs::read_to_string(&path)?;
            let o: RepoOverrides = toml::from_str(&text).context("parsing lyra.toml")?;
            if let Some(v) = o.verify {
                self.repository_verify = v;
            }
            self.repository_profiles = o.profiles.into_iter().filter_map(|(id, p)| p.verify.map(|v| (id, v))).collect();
        }
        Ok(self)
    }

    /// The verify commands for a run with this execution profile; `None` leaves the step to
    /// the profile's default. Precedence: repository `[profiles.<id>.verify]`, repository
    /// `[verify]` (`rust-cargo` only), operator `[profiles.<id>.verify]`, operator `[verify]`
    /// (`rust-cargo` only). The unscoped `[verify]` tables predate execution profiles and hold
    /// Cargo commands, so they never reach another profile.
    pub fn verify_commands(&self, profile_id: &str) -> VerifyCommands {
        let rust = profile_id == crate::profile::rust_cargo::ID;
        let repo_profile = self.repository_profiles.get(profile_id).cloned().unwrap_or_default();
        let repo_legacy = if rust { self.repository_verify.clone() } else { VerifyCommands::default() };
        let operator = self.profiles.get(profile_id).map(|p| p.verify.clone()).unwrap_or_default();
        let legacy = if rust {
            VerifyCommands {
                build: Some(self.verify.build.clone()),
                test: Some(self.verify.test.clone()),
                fmt: Some(self.verify.fmt.clone()),
            }
        } else {
            VerifyCommands::default()
        };
        // Highest precedence first; for each command the first level that sets it wins.
        let levels = [&repo_profile, &repo_legacy, &operator, &legacy];
        let pick = |field: fn(&VerifyCommands) -> &Option<String>| levels.iter().find_map(|l| field(l).clone());
        VerifyCommands { build: pick(|v| &v.build), test: pick(|v| &v.test), fmt: pick(|v| &v.fmt) }
    }

    /// One line describing the verify commands a profile will use, for the run log.
    pub fn describe_verify_commands(&self, profile_id: &str) -> String {
        let v = self.verify_commands(profile_id);
        let show = |c: &Option<String>| c.clone().unwrap_or_else(|| "profile default".into());
        format!("build={}; test={}; fmt={}", show(&v.build), show(&v.test), show(&v.fmt))
    }

    pub fn provider(&self, id: &str) -> Result<&ProviderConfig> {
        self.providers.get(id).with_context(|| format!("unknown provider '{id}'"))
    }

    pub fn timeout(&self) -> std::time::Duration {
        std::time::Duration::from_secs(self.run.timeout_minutes * 60)
    }
}

pub const DEFAULT_CONFIG_TOML: &str = r#"# LYRA configuration (see docs/PLAN.md §7)

[run]
provider = "claude"            # "claude" | "codex"
timeout_minutes = 30           # hang guard per agent call / verify command

[server]
bind = "127.0.0.1:7700"
open_browser = true

[providers.claude]
kind = "claude"
auth = "api_key"               # "api_key" (needs ANTHROPIC_API_KEY) | "login" (uses `claude` login)
model = "opus"

[providers.codex]
kind = "codex"
# model = "gpt-5.5-codex"

[verify]                       # rust-cargo repositories; <repo>/lyra.toml [verify] overrides any profile
build = "cargo build --workspace"
test  = "cargo test --workspace"
fmt   = "cargo fmt --all -- --check"

# python-pytest repositories: the regression test and `python3 -m pytest` always run;
# build and fmt run only when set. LYRA uses the python3 on PATH (activate your venv first).
# [profiles.python-pytest.verify]
# test = "python3 -m pytest"
# fmt  = "ruff format --check ."
"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_toml_parses_and_matches_defaults() {
        let cfg = Config::parse(DEFAULT_CONFIG_TOML).unwrap();
        assert_eq!(cfg.run.provider, "claude");
        assert_eq!(cfg.run.timeout_minutes, 30);
        assert_eq!(cfg.providers["claude"].kind, ProviderKind::Claude);
        assert_eq!(cfg.providers["claude"].auth, ClaudeAuth::ApiKey);
        assert_eq!(cfg.providers["codex"].kind, ProviderKind::Codex);
        assert_eq!(cfg.verify.test, "cargo test --workspace");
    }

    #[test]
    fn unknown_provider_is_rejected() {
        let err = Config::parse("[run]\nprovider = \"nope\"\n").unwrap_err();
        assert!(err.to_string().contains("nope"));
    }

    #[test]
    fn repo_overrides_only_touch_verify() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("lyra.toml"),
            "[verify]\ntest = \"cargo test -p core\"\n[run]\nprovider = \"codex\"\n",
        )
        .unwrap();
        let cfg = Config::default().with_repo_overrides(dir.path()).unwrap();
        let rust = cfg.verify_commands("rust-cargo");
        assert_eq!(rust.test.as_deref(), Some("cargo test -p core"));
        assert_eq!(rust.build.as_deref(), Some("cargo build --workspace"));
        assert_eq!(cfg.run.provider, "claude");
        // The repository's unscoped table holds Cargo commands: it applies to rust-cargo only,
        // and neither it nor the Cargo defaults leak into another profile.
        let python = cfg.verify_commands("python-pytest");
        assert_eq!((python.build, python.test, python.fmt), (None, None, None));
    }

    #[test]
    fn profile_tables_sit_between_repository_and_legacy_settings() {
        let cfg = Config::parse(
            "[verify]\ntest = \"cargo nextest run\"\n\
             [profiles.python-pytest.verify]\ntest = \"python3 -m pytest -q tests\"\nfmt = \"ruff format --check .\"\n\
             [profiles.rust-cargo.verify]\nfmt = \"true\"\n",
        )
        .unwrap();
        let python = cfg.verify_commands("python-pytest");
        assert_eq!(python.test.as_deref(), Some("python3 -m pytest -q tests"));
        assert_eq!(python.fmt.as_deref(), Some("ruff format --check ."));
        assert_eq!(python.build, None);
        let rust = cfg.verify_commands("rust-cargo");
        assert_eq!(rust.test.as_deref(), Some("cargo nextest run"));
        assert_eq!(rust.fmt.as_deref(), Some("true"), "a profile table beats the legacy table");
        assert_eq!(rust.build.as_deref(), Some("cargo build --workspace"));
        // A repository's per-profile table wins for that profile; its unscoped `[verify]` holds
        // Cargo commands and applies to rust-cargo only, so a mixed repository run with the
        // Python profile never runs `cargo test` as its suite.
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("lyra.toml"),
            "[verify]\ntest = \"cargo test --workspace -p core\"\n\
             [profiles.python-pytest]\npackages = [\"app\"]\n\
             [profiles.python-pytest.verify]\ntest = \"python3 -m pytest tests/unit\"\n",
        )
        .unwrap();
        let with_repo = cfg.clone().with_repo_overrides(dir.path()).unwrap();
        assert_eq!(with_repo.verify_commands("python-pytest").test.as_deref(), Some("python3 -m pytest tests/unit"));
        assert_eq!(with_repo.verify_commands("rust-cargo").test.as_deref(), Some("cargo test --workspace -p core"));
        std::fs::write(dir.path().join("lyra.toml"), "[verify]\ntest = \"cargo test --workspace\"\n").unwrap();
        let mixed = cfg.clone().with_repo_overrides(dir.path()).unwrap();
        assert_eq!(
            mixed.verify_commands("python-pytest").test.as_deref(),
            Some("python3 -m pytest -q tests"),
            "the repository's Cargo commands never reach the Python profile"
        );
        assert_eq!(mixed.verify_commands("rust-cargo").test.as_deref(), Some("cargo test --workspace"));
        assert_eq!(
            Config::default().describe_verify_commands("python-pytest"),
            "build=profile default; test=profile default; fmt=profile default"
        );
        assert!(Config::parse(DEFAULT_CONFIG_TOML).unwrap().profiles.is_empty());
    }

    #[test]
    fn load_falls_back_to_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = Config::load(dir.path()).unwrap();
        assert_eq!(cfg, Config::default());
        let path = Config::ensure_default(dir.path()).unwrap();
        assert!(path.exists());
    }
}
