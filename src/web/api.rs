//! JSON API: `/api/runs`, `/api/runs/{id}`, cancel, retry, artifacts, doctor, config.
//! Errors are `{"error": "..."}` with 400 (bad input), 404 (unknown run/file) or 409 (busy).

use super::server::AppState;
use crate::engine::Engine;
use crate::events::StageId;
use crate::state::{RunRequest, RunState};
use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::Deserialize;
use serde_json::json;
use std::fs::File;
use std::io::{self, Read};
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

/// Filesystem scans and retry's git commands must not occupy an async worker.
/// Entering the runtime also lets Engine::submit/retry spawn the background driver.
pub async fn blocking<T: Send + 'static>(f: impl FnOnce() -> ApiResult<T> + Send + 'static) -> ApiResult<T> {
    let runtime = tokio::runtime::Handle::current();
    tokio::task::spawn_blocking(move || {
        let _entered = runtime.enter();
        f()
    })
    .await
    .map_err(|e| ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, format!("background task failed: {e}")))?
}

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
        // Win32 can normalize these names into another component (including
        // `.. ` into its parent), so never pass them to a pathname-based open.
        #[cfg(windows)]
        if part.ends_with(['.', ' ']) {
            return None;
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
    if !valid_run_id(id) || !run_dir(engine, id)?.root.join("run.json").exists() {
        return Err(ApiError::not_found(format!("no run '{id}'")));
    }
    engine.get(id).map_err(|e| ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, format!("reading run '{id}': {e:#}")))
}

pub fn run_dir(engine: &Engine, id: &str) -> ApiResult<crate::artifacts::RunDir> {
    engine.run_dir(id).map_err(|_| ApiError::not_found("no such run"))
}

/// Makes the repo path absolute (like the CLI) without Windows' `\\?\` verbatim prefix.
pub fn normalize_repo(repo: PathBuf) -> PathBuf {
    dunce::canonicalize(&repo).unwrap_or(repo)
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

pub fn retry(engine: &std::sync::Arc<Engine>, id: &str, stage: &str, acknowledge_policy: bool) -> ApiResult<StageId> {
    let from = StageId::parse(stage).ok_or_else(|| ApiError::bad_request(format!("unknown stage '{stage}'")))?;
    let st = load_run(engine, id)?;
    if !st.status.is_terminal() || engine.is_busy() {
        return Err(ApiError::conflict("a run is active; wait for it to finish or cancel it"));
    }
    engine.retry_with_policy_ack(id, from, acknowledge_policy).map_err(|e| ApiError::bad_request(format!("{e:#}")))?;
    Ok(from)
}

// ---- handlers ----

pub async fn create_run(State(s): State<AppState>, Json(req): Json<RunRequest>) -> ApiResult<Response> {
    let id = blocking(move || submit(&s.engine, req)).await?;
    Ok((StatusCode::CREATED, Json(json!({ "id": id }))).into_response())
}

pub async fn list_runs(State(s): State<AppState>) -> ApiResult<Json<Vec<RunState>>> {
    blocking(move || Ok(Json(s.engine.list()))).await
}

pub async fn get_run(State(s): State<AppState>, Path(id): Path<String>) -> ApiResult<Json<RunState>> {
    blocking(move || load_run(&s.engine, &id).map(Json)).await
}

pub async fn cancel_run(State(s): State<AppState>, Path(id): Path<String>) -> ApiResult<Json<serde_json::Value>> {
    let cancel_id = id.clone();
    blocking(move || cancel(&s.engine, &cancel_id)).await?;
    Ok(Json(json!({ "id": id, "cancelled": true })))
}

#[derive(Default, Deserialize)]
pub struct RetryRequest {
    #[serde(default)]
    pub policy_acknowledged: bool,
}

pub async fn retry_run(
    State(s): State<AppState>,
    Path((id, stage)): Path<(String, String)>,
    body: Bytes,
) -> ApiResult<Json<serde_json::Value>> {
    // Existing API clients post no body; an optional JSON body may acknowledge
    // a policy discovered by intake without recreating the run.
    let request: RetryRequest = if body.is_empty() {
        RetryRequest::default()
    } else {
        serde_json::from_slice(&body).map_err(|e| ApiError::bad_request(format!("invalid retry request: {e}")))?
    };
    let retry_id = id.clone();
    let from = blocking(move || retry(&s.engine, &retry_id, &stage, request.policy_acknowledged)).await?;
    Ok(Json(json!({ "id": id, "from": from })))
}

/// `GET /api/runs/{id}/artifacts/{*path}` and `GET /runs/{id}/artifacts/{*path}`: text/plain.
pub async fn artifact(State(s): State<AppState>, Path((id, path)): Path<(String, String)>) -> ApiResult<Response> {
    let text = blocking(move || read_artifact(&s.engine, &id, &path)).await?;
    Ok(([(header::CONTENT_TYPE, "text/plain; charset=utf-8"), (header::X_CONTENT_TYPE_OPTIONS, "nosniff")], text)
        .into_response())
}

pub fn read_artifact(engine: &Engine, id: &str, path: &str) -> ApiResult<String> {
    load_run(engine, id)?;
    let rel =
        safe_artifact_path(path).ok_or_else(|| ApiError::bad_request(format!("invalid artifact path '{path}'")))?;
    let opened = open_artifact(engine.home(), id, &rel).map_err(|_| ApiError::not_found("artifact is unavailable"))?;
    opened.read_text().map_err(|e| {
        if e.kind() == io::ErrorKind::InvalidData {
            ApiError::bad_request("artifact is not UTF-8 text")
        } else {
            ApiError::not_found("artifact is unavailable")
        }
    })
}

/// Read the same file whose handle metadata was checked. Directory handles stay
/// alive through the read; on Windows they also deny rename/delete while pinned.
struct OpenArtifact {
    file: File,
    _directories: Vec<File>,
}

impl OpenArtifact {
    fn read_text(mut self) -> io::Result<String> {
        check_artifact_file(&self.file)?;
        let mut text = String::new();
        self.file.read_to_string(&mut text)?;
        check_artifact_file(&self.file)?;
        Ok(text)
    }
}

fn unsafe_artifact() -> io::Error {
    io::Error::new(io::ErrorKind::PermissionDenied, "artifact must be a regular file without links or reparse points")
}

#[cfg(unix)]
fn open_artifact(home: &std::path::Path, id: &str, rel: &std::path::Path) -> io::Result<OpenArtifact> {
    use std::ffi::CString;
    use std::os::fd::{AsRawFd, FromRawFd};
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::OpenOptionsExt;

    // The configured home is the trusted anchor. Every requested descendant is
    // opened relative to its pinned parent, never by reopening a checked path.
    let anchor = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(home)?;
    let mut directories = vec![anchor];
    let relative = PathBuf::from("runs").join(id).join("artifacts").join(rel);
    let mut parts = relative.components().peekable();
    while let Some(part) = parts.next() {
        let Component::Normal(part) = part else { return Err(unsafe_artifact()) };
        let part = CString::new(part.as_bytes()).map_err(|_| unsafe_artifact())?;
        let last = parts.peek().is_none();
        let flags = libc::O_RDONLY
            | libc::O_NOFOLLOW
            | libc::O_CLOEXEC
            | if last { libc::O_NONBLOCK } else { libc::O_DIRECTORY };
        // SAFETY: the parent FD and NUL-terminated component remain valid for
        // this call. No creation flag is present, so openat needs no mode arg.
        let fd = unsafe { libc::openat(directories.last().unwrap().as_raw_fd(), part.as_ptr(), flags) };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: openat returned a new owned FD; this is its only owner.
        let file = unsafe { File::from_raw_fd(fd) };
        if last {
            return Ok(OpenArtifact { file, _directories: directories });
        }
        directories.push(file);
    }
    Err(unsafe_artifact())
}

#[cfg(unix)]
fn check_artifact_file(file: &File) -> io::Result<()> {
    use std::os::unix::fs::MetadataExt;
    let metadata = file.metadata()?;
    if !metadata.is_file() || metadata.nlink() != 1 {
        return Err(unsafe_artifact());
    }
    Ok(())
}

#[cfg(windows)]
fn windows_file_information(
    file: &File,
) -> io::Result<windows_sys::Win32::Storage::FileSystem::BY_HANDLE_FILE_INFORMATION> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Storage::FileSystem::GetFileInformationByHandle;
    let mut information = std::mem::MaybeUninit::uninit();
    // SAFETY: File owns a live handle and the output points to writable storage
    // for BY_HANDLE_FILE_INFORMATION. A successful call initializes it fully.
    if unsafe { GetFileInformationByHandle(file.as_raw_handle(), information.as_mut_ptr()) } == 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: the successful call initialized every field.
    Ok(unsafe { information.assume_init() })
}

#[cfg(windows)]
fn open_artifact(home: &std::path::Path, id: &str, rel: &std::path::Path) -> io::Result<OpenArtifact> {
    use std::os::windows::fs::OpenOptionsExt;
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_REPARSE_POINT, FILE_FLAG_BACKUP_SEMANTICS,
        FILE_FLAG_OPEN_REPARSE_POINT, FILE_READ_ATTRIBUTES, FILE_SHARE_READ, FILE_SHARE_WRITE,
    };

    let pin_directory = |path: &std::path::Path| -> io::Result<File> {
        let file = std::fs::OpenOptions::new()
            .access_mode(FILE_READ_ATTRIBUTES)
            // Deny delete/rename and write handles (which could install a
            // reparse point in place) until the final read finishes. Otherwise a
            // checked ancestor could redirect a later absolute child open.
            .share_mode(FILE_SHARE_READ)
            .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT | FILE_FLAG_BACKUP_SEMANTICS)
            .open(path)?;
        let attributes = windows_file_information(&file)?.dwFileAttributes;
        if attributes & (FILE_ATTRIBUTE_DIRECTORY | FILE_ATTRIBUTE_REPARSE_POINT) != FILE_ATTRIBUTE_DIRECTORY {
            return Err(unsafe_artifact());
        }
        Ok(file)
    };
    let mut directories = Vec::new();
    let mut candidate = PathBuf::new();
    // Pin the complete absolute ancestor chain too: OPEN_REPARSE_POINT applies
    // only to the final component of each CreateFile call.
    for part in std::path::absolute(home)?.components() {
        match part {
            Component::Prefix(_) => candidate.push(part),
            Component::RootDir | Component::Normal(_) => {
                candidate.push(part);
                directories.push(pin_directory(&candidate)?);
            }
            Component::CurDir => {}
            Component::ParentDir => return Err(unsafe_artifact()),
        }
    }
    let relative = PathBuf::from("runs").join(id).join("artifacts").join(rel);
    let mut parts = relative.components().peekable();
    while let Some(part) = parts.next() {
        let Component::Normal(part) = part else { return Err(unsafe_artifact()) };
        candidate.push(part);
        if parts.peek().is_none() {
            let file = std::fs::OpenOptions::new()
                .read(true)
                .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
                .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
                .open(&candidate)?;
            return Ok(OpenArtifact { file, _directories: directories });
        }
        directories.push(pin_directory(&candidate)?);
    }
    Err(unsafe_artifact())
}

#[cfg(windows)]
fn check_artifact_file(file: &File) -> io::Result<()> {
    use windows_sys::Win32::Storage::FileSystem::{FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_REPARSE_POINT};
    let information = windows_file_information(file)?;
    if information.dwFileAttributes & (FILE_ATTRIBUTE_DIRECTORY | FILE_ATTRIBUTE_REPARSE_POINT) != 0
        || information.nNumberOfLinks != 1
        || !file.metadata()?.is_file()
    {
        return Err(unsafe_artifact());
    }
    Ok(())
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
    use crate::config::Config;

    fn run_home() -> (tempfile::TempDir, std::sync::Arc<Engine>) {
        let home = tempfile::tempdir().unwrap();
        let engine = Engine::new(home.path().to_path_buf(), Config::default());
        let rd = engine.run_dir("r1").unwrap();
        rd.create().unwrap();
        let req = RunRequest {
            repo: home.path().to_path_buf(),
            issue: "#1".into(),
            issue_text: None,
            provider: None,
            policy_acknowledged: false,
        };
        RunState::new("r1".into(), req, "claude".into()).save(&rd.root).unwrap();
        (home, engine)
    }

    #[test]
    fn hardlinked_files_cannot_be_downloaded_as_artifacts() {
        let (_home, engine) = run_home();
        let outside = tempfile::tempdir().unwrap();
        let secret = outside.path().join("secret.txt");
        std::fs::write(&secret, "secret-not-an-artifact").unwrap();
        let artifact = engine.run_dir("r1").unwrap().artifacts().join("20-repro/leak.txt");
        std::fs::hard_link(secret, artifact).unwrap();
        let err = read_artifact(&engine, "r1", "20-repro/leak.txt").unwrap_err();
        assert_eq!(err.status, StatusCode::NOT_FOUND);
    }

    #[test]
    fn replacing_an_opened_artifact_cannot_change_the_handle_being_read() {
        let (_home, engine) = run_home();
        let rd = engine.run_dir("r1").unwrap();
        rd.write("20-repro/output.txt", "validated artifact").unwrap();
        let path = rd.artifacts().join("20-repro/output.txt");
        let moved = rd.artifacts().join("20-repro/original.txt");
        let opened = open_artifact(engine.home(), "r1", std::path::Path::new("20-repro/output.txt")).unwrap();
        #[cfg(unix)]
        {
            std::fs::rename(&path, &moved).unwrap();
            let outside = tempfile::tempdir().unwrap();
            let secret = outside.path().join("secret.txt");
            std::fs::write(&secret, "secret-not-an-artifact").unwrap();
            std::os::unix::fs::symlink(secret, &path).unwrap();
        }
        #[cfg(windows)]
        assert!(std::fs::rename(&path, &moved).is_err(), "the opened file must deny rename/delete");
        assert_eq!(opened.read_text().unwrap(), "validated artifact");
        #[cfg(windows)]
        std::fs::rename(path, moved).unwrap();
    }

    #[test]
    fn opened_directory_chain_cannot_be_redirected_before_the_read() {
        let (_home, engine) = run_home();
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("output.txt"), "secret-not-an-artifact").unwrap();
        let rd = engine.run_dir("r1").unwrap();
        rd.write("20-repro/output.txt", "validated artifact").unwrap();
        let directory = rd.artifacts().join("20-repro");
        let moved = rd.artifacts().join("held-repro");
        let opened = open_artifact(engine.home(), "r1", std::path::Path::new("20-repro/output.txt")).unwrap();
        // Reading an artifact must still allow the harness to write other files.
        rd.write("20-repro/new.txt", "new evidence").unwrap();
        #[cfg(unix)]
        {
            std::fs::rename(&directory, &moved).unwrap();
            std::os::unix::fs::symlink(outside.path(), &directory).unwrap();
        }
        #[cfg(windows)]
        assert!(std::fs::rename(&directory, &moved).is_err(), "checked ancestors must deny rename/delete");
        assert_eq!(opened.read_text().unwrap(), "validated artifact");
        #[cfg(windows)]
        {
            std::fs::rename(&directory, moved).unwrap();
            let result = std::process::Command::new("cmd.exe")
                .args(["/C", "mklink", "/J"])
                .arg(&directory)
                .arg(outside.path())
                .output()
                .unwrap();
            assert!(result.status.success(), "{}", String::from_utf8_lossy(&result.stderr));
        }
        assert_eq!(read_artifact(&engine, "r1", "20-repro/output.txt").unwrap_err().status, StatusCode::NOT_FOUND);
    }

    #[cfg(unix)]
    #[test]
    fn nonregular_artifact_cannot_block_the_reader() {
        use std::ffi::CString;
        use std::os::unix::ffi::OsStrExt;
        let (_home, engine) = run_home();
        let path = engine.run_dir("r1").unwrap().artifacts().join("pipe.txt");
        let name = CString::new(path.as_os_str().as_bytes()).unwrap();
        // SAFETY: name is a live, NUL-terminated path under the temporary home.
        assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
        assert_eq!(read_artifact(&engine, "r1", "pipe.txt").unwrap_err().status, StatusCode::NOT_FOUND);
    }

    #[test]
    fn safe_artifact_path_rejects_traversal() {
        assert_eq!(safe_artifact_path("20-repro/output.txt"), Some(PathBuf::from("20-repro").join("output.txt")));
        assert_eq!(safe_artifact_path("/00-issue.md"), Some(PathBuf::from("00-issue.md")));
        assert_eq!(safe_artifact_path("./a//b"), Some(PathBuf::from("a").join("b")));
        for bad in ["..", "../run.json", "a/../../x", "a\\..\\x", "", "/", "C:\\x", "C:x", "a/\0"] {
            assert_eq!(safe_artifact_path(bad), None, "{bad:?} should be rejected");
        }
        #[cfg(windows)]
        for bad in [".. /run.json", ".../run.json", "name.", "name "] {
            assert_eq!(safe_artifact_path(bad), None, "Win32 must not normalize {bad:?}");
        }
    }

    #[test]
    fn run_ids_are_restricted() {
        assert!(valid_run_id("my-crate-42-20261104-101500"));
        assert!(!valid_run_id(".."));
        assert!(!valid_run_id("a/b"));
        assert!(!valid_run_id(""));
    }

    #[test]
    fn artifact_directory_links_cannot_escape_the_run() {
        let home = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let engine = Engine::new(home.path().to_path_buf(), Config::default());
        let rd = engine.run_dir("r1").unwrap();
        rd.create().unwrap();
        let req = RunRequest {
            repo: home.path().to_path_buf(),
            issue: "#1".into(),
            issue_text: None,
            provider: None,
            policy_acknowledged: false,
        };
        RunState::new("r1".into(), req, "claude".into()).save(&rd.root).unwrap();
        std::fs::write(outside.path().join("secret.txt"), "secret-not-an-artifact").unwrap();
        let link = rd.artifacts().join("escape");
        #[cfg(unix)]
        std::os::unix::fs::symlink(outside.path(), &link).unwrap();
        #[cfg(windows)]
        {
            // Junctions work without Windows developer mode or administrator rights.
            let result = std::process::Command::new("cmd.exe")
                .args(["/C", "mklink", "/J"])
                .arg(&link)
                .arg(outside.path())
                .output()
                .unwrap();
            assert!(result.status.success(), "{}", String::from_utf8_lossy(&result.stderr));
        }
        let err = read_artifact(&engine, "r1", "escape/secret.txt").unwrap_err();
        assert_eq!(err.status, StatusCode::NOT_FOUND);
        rd.write("safe.txt", "ordinary artifact").unwrap();
        assert_eq!(read_artifact(&engine, "r1", "safe.txt").unwrap(), "ordinary artifact");
    }
}
