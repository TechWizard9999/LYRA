//! JSON API: `/api/runs`, `/api/runs/{id}`, cancel, retry, artifacts, doctor, config.
//! Errors are `{"error": "..."}` with 400 (bad input), 404 (unknown run/file) or 409 (busy).

use super::server::AppState;
use crate::engine::Engine;
use crate::events::StageId;
use crate::state::{RunRequest, RunState};
use axum::extract::{Path, State};
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::json;
use std::path::{Component, PathBuf};

#[derive(Debug)]
pub struct ApiError {
    pub status: StatusCode,
    pub message: String,
}

impl ApiError {
    pub fn new(status: StatusCode, message: impl Into<String>) -> Self {
        Self { status, message: message.into() }
    }
    pub fn bad_request(message: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, message)
    }
    pub fn not_found(message: impl Into<String>) -> Self {
        Self::new(StatusCode::NOT_FOUND, message)
    }
    pub fn conflict(message: impl Into<String>) -> Self {
        Self::new(StatusCode::CONFLICT, message)
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.status, Json(json!({ "error": self.message }))).into_response()
    }
}

pub type ApiResult<T> = Result<T, ApiError>;

/// Run ids are generated as `[a-z0-9-]+`; anything else (e.g. `..`) never names a run.
pub fn valid_run_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= 200 && id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

/// Validates a path relative to `artifacts/`: only normal components, no `..`, no root,
/// no drive prefix. Accepts `/` and `\` as separators.
pub fn safe_artifact_path(rel: &str) -> Option<PathBuf> {
    let rel = rel.trim_start_matches('/');
    if rel.is_empty() || rel.contains('\0') {
        return None;
    }
    let mut out = PathBuf::new();
    for part in rel.split(['/', '\\']) {
        if part.is_empty() || part == "." {
            continue;
        }
        let mut comps = std::path::Path::new(part).components();
        match (comps.next(), comps.next()) {
            (Some(Component::Normal(c)), None) if part != ".." && !part.contains(':') => out.push(c),
            _ => return None,
        }
    }
    if out.as_os_str().is_empty() {
        None
    } else {
        Some(out)
    }
}

/// Loads a run, distinguishing "no such run" (404) from a broken `run.json` (500).
pub fn load_run(engine: &Engine, id: &str) -> ApiResult<RunState> {
    if !valid_run_id(id) || !engine.run_dir(id).root.join("run.json").exists() {
        return Err(ApiError::not_found(format!("no run '{id}'")));
    }
    engine.get(id).map_err(|e| ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, format!("reading run '{id}': {e:#}")))
}

/// Makes the repo path absolute (like the CLI) without Windows' `\\?\` verbatim prefix.
pub fn normalize_repo(repo: PathBuf) -> PathBuf {
    match repo.canonicalize() {
        Ok(p) => {
            let s = p.to_string_lossy();
            match s.strip_prefix(r"\\?\") {
                Some(rest) if !rest.starts_with("UNC\\") => PathBuf::from(rest),
                _ => p,
            }
        }
        Err(_) => repo,
    }
}

/// Shared by the form and the API: validates, submits, maps engine errors to statuses.
pub fn submit(engine: &std::sync::Arc<Engine>, mut req: RunRequest) -> ApiResult<String> {
    if req.repo.as_os_str().is_empty() {
        return Err(ApiError::bad_request("repo path is required"));
    }
    if req.issue.trim().is_empty() {
        return Err(ApiError::bad_request("issue is required (URL, #123 or 123)"));
    }
    if !req.repo.is_dir() {
        return Err(ApiError::bad_request(format!("repo path {} is not a directory", req.repo.display())));
    }
    req.repo = normalize_repo(req.repo);
    req.issue = req.issue.trim().to_string();
    if engine.is_busy() {
        return Err(ApiError::conflict("a run is already active; wait for it to finish or cancel it"));
    }
    engine.submit(req).map_err(|e| {
        if engine.is_busy() {
            ApiError::conflict(format!("{e:#}"))
        } else {
            ApiError::bad_request(format!("{e:#}"))
        }
    })
}

pub fn cancel(engine: &Engine, id: &str) -> ApiResult<()> {
    load_run(engine, id)?;
    if engine.cancel(id) {
        Ok(())
    } else {
        Err(ApiError::conflict(format!("run '{id}' is not active")))
    }
}

pub fn retry(engine: &std::sync::Arc<Engine>, id: &str, stage: &str) -> ApiResult<StageId> {
    let from = StageId::parse(stage).ok_or_else(|| ApiError::bad_request(format!("unknown stage '{stage}'")))?;
    let st = load_run(engine, id)?;
    if !st.status.is_terminal() || engine.is_busy() {
        return Err(ApiError::conflict("a run is active; wait for it to finish or cancel it"));
    }
    engine.retry(id, from).map_err(|e| ApiError::bad_request(format!("{e:#}")))?;
    Ok(from)
}

// ---- handlers ----

pub async fn create_run(State(s): State<AppState>, Json(req): Json<RunRequest>) -> ApiResult<Response> {
    let id = submit(&s.engine, req)?;
    Ok((StatusCode::CREATED, Json(json!({ "id": id }))).into_response())
}

pub async fn list_runs(State(s): State<AppState>) -> Json<Vec<RunState>> {
    Json(s.engine.list())
}

pub async fn get_run(State(s): State<AppState>, Path(id): Path<String>) -> ApiResult<Json<RunState>> {
    load_run(&s.engine, &id).map(Json)
}

pub async fn cancel_run(State(s): State<AppState>, Path(id): Path<String>) -> ApiResult<Json<serde_json::Value>> {
    cancel(&s.engine, &id)?;
    Ok(Json(json!({ "id": id, "cancelled": true })))
}

pub async fn retry_run(
    State(s): State<AppState>,
    Path((id, stage)): Path<(String, String)>,
) -> ApiResult<Json<serde_json::Value>> {
    let from = retry(&s.engine, &id, &stage)?;
    Ok(Json(json!({ "id": id, "from": from })))
}

/// `GET /api/runs/{id}/artifacts/{*path}` and `GET /runs/{id}/artifacts/{*path}`: text/plain.
pub async fn artifact(State(s): State<AppState>, Path((id, path)): Path<(String, String)>) -> ApiResult<Response> {
    load_run(&s.engine, &id)?;
    let rel =
        safe_artifact_path(&path).ok_or_else(|| ApiError::bad_request(format!("invalid artifact path '{path}'")))?;
    let rd = s.engine.run_dir(&id);
    if !rd.artifacts().join(&rel).is_file() {
        return Err(ApiError::not_found(format!("no artifact '{path}'")));
    }
    let text = rd.read(&rel).map_err(|e| ApiError::bad_request(format!("{e:#} (not a text file?)")))?;
    Ok(([(header::CONTENT_TYPE, "text/plain; charset=utf-8")], text).into_response())
}

pub async fn doctor(State(s): State<AppState>) -> Json<serde_json::Value> {
    let report = crate::doctor::run(s.engine.config()).await;
    let lines: Vec<_> = report.lines.iter().map(|(ok, text)| json!({ "ok": ok, "text": text })).collect();
    Json(json!({ "ok": report.ok(), "lines": lines }))
}

pub async fn config(State(s): State<AppState>) -> Json<crate::config::Config> {
    Json(s.engine.config().clone())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn safe_artifact_path_rejects_traversal() {
        assert_eq!(safe_artifact_path("20-repro/output.txt"), Some(PathBuf::from("20-repro").join("output.txt")));
        assert_eq!(safe_artifact_path("/00-issue.md"), Some(PathBuf::from("00-issue.md")));
        assert_eq!(safe_artifact_path("./a//b"), Some(PathBuf::from("a").join("b")));
        for bad in ["..", "../run.json", "a/../../x", "a\\..\\x", "", "/", "C:\\x", "C:x", "a/\0"] {
            assert_eq!(safe_artifact_path(bad), None, "{bad:?} should be rejected");
        }
    }

    #[test]
    fn run_ids_are_restricted() {
        assert!(valid_run_id("my-crate-42-20261104-101500"));
        assert!(!valid_run_id(".."));
        assert!(!valid_run_id("a/b"));
        assert!(!valid_run_id(""));
    }
}
