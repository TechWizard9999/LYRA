//! Router, shared state, HTML page handlers and the compiled-in static assets.

use super::api::{self, load_run, ApiError};
use super::sse;
use super::views::{self, HomeView, NewRunForm, RunView};
use crate::engine::Engine;
use crate::state::RunRequest;
use axum::extract::{Path, Query, Request, State};
use axum::http::{header, Method, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{Html, IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use axum::{Form, Router};
use serde::Deserialize;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

const APP_CSS: &str = include_str!("static/app.css");
const APP_JS: &str = include_str!("static/app.js");

#[derive(Clone)]
pub struct AppState {
    pub engine: Arc<Engine>,
    pub address: SocketAddr,
}

impl AppState {
    pub fn new(engine: Arc<Engine>, address: SocketAddr) -> Self {
        Self { engine, address }
    }
}

/// Browsers can send requests to localhost from an unrelated site. Validate Host
/// before reads (DNS rebinding), and require an exact same-origin Origin on writes.
/// Non-browser API clients must send that Origin too; a missing header is not trusted.
fn request_allowed(req: &Request, address: SocketAddr) -> bool {
    let Some(host) = req.headers().get(header::HOST).and_then(|h| h.to_str().ok()) else { return false };
    let allowed_hosts = [
        format!("127.0.0.1:{}", address.port()),
        format!("localhost:{}", address.port()),
        format!("[::1]:{}", address.port()),
        address.to_string(),
    ];
    if !address.ip().is_loopback() || !allowed_hosts.iter().any(|h| h == host) {
        return false;
    }
    if let Some(site) = req.headers().get("sec-fetch-site") {
        if !matches!(site.to_str(), Ok("same-origin" | "none")) {
            return false;
        }
    }
    let expected = format!("http://{host}");
    if let Some(origin) = req.headers().get(header::ORIGIN) {
        if origin.to_str().ok() != Some(expected.as_str()) {
            return false;
        }
    } else if !matches!(*req.method(), Method::GET | Method::HEAD) {
        return false;
    }
    true
}

async fn protect_localhost(State(s): State<AppState>, req: Request, next: Next) -> Response {
    if !request_allowed(&req, s.address) {
        return (StatusCode::FORBIDDEN, "request must come from this loopback dashboard origin").into_response();
    }
    let mut response = next.run(req).await;
    response.headers_mut().insert(header::X_CONTENT_TYPE_OPTIONS, "nosniff".parse().unwrap());
    response.headers_mut().insert(header::CACHE_CONTROL, "no-store".parse().unwrap());
    // `no-referrer` can serialize a form POST's Origin as "null". Keep
    // same-origin form requests intact while suppressing cross-origin referrers.
    response.headers_mut().insert(header::REFERRER_POLICY, "same-origin".parse().unwrap());
    response.headers_mut().insert(
        header::CONTENT_SECURITY_POLICY,
        "default-src 'self'; frame-ancestors 'none'; form-action 'self'; base-uri 'none'".parse().unwrap(),
    );
    response
}

pub fn router(state: AppState) -> Router {
    Router::new()
        // HTML
        .route("/", get(home))
        .route("/runs", post(create_run_form))
        .route("/runs/{id}", get(run_page))
        .route("/runs/{id}/cancel", post(cancel_form))
        .route("/runs/{id}/retry/{stage}", post(retry_form))
        .route("/runs/{id}/artifacts/{*path}", get(api::artifact))
        .route("/static/app.css", get(|| async { ([(header::CONTENT_TYPE, "text/css; charset=utf-8")], APP_CSS) }))
        .route("/static/app.js", get(|| async { ([(header::CONTENT_TYPE, "text/javascript; charset=utf-8")], APP_JS) }))
        // JSON API
        .route("/api/runs", post(api::create_run).get(api::list_runs))
        .route("/api/runs/{id}", get(api::get_run))
        .route("/api/runs/{id}/events", get(sse::events))
        .route("/api/runs/{id}/cancel", post(api::cancel_run))
        .route("/api/runs/{id}/retry/{stage}", post(api::retry_run))
        .route("/api/runs/{id}/artifacts/{*path}", get(api::artifact))
        .route("/api/doctor", post(api::doctor))
        .route("/api/config", get(api::config))
        .layer(middleware::from_fn_with_state(state.clone(), protect_localhost))
        .with_state(state)
}

fn render_home(engine: &Engine, form: &NewRunForm, error: Option<&str>) -> String {
    let runs = engine.list();
    let cfg = engine.config();
    let view = HomeView {
        runs: &runs,
        providers: cfg.providers.keys().cloned().collect(),
        default_provider: &cfg.run.provider,
        form,
        error,
        busy: engine.is_busy(),
    };
    views::home(&view).into_string()
}

async fn home(State(s): State<AppState>) -> Result<Html<String>, ApiError> {
    api::blocking(move || Ok(Html(render_home(&s.engine, &NewRunForm::default(), None)))).await
}

async fn create_run_form(State(s): State<AppState>, Form(f): Form<NewRunForm>) -> Response {
    let text = f.issue_text.trim();
    let req = RunRequest {
        repo: PathBuf::from(f.repo.trim()),
        issue: f.issue.clone(),
        issue_text: (!text.is_empty()).then(|| text.to_string()),
        provider: (!f.provider.is_empty()).then(|| f.provider.clone()),
        policy_acknowledged: f.policy_acknowledged.is_some(),
    };
    let engine = s.engine.clone();
    match api::blocking(move || api::submit(&engine, req)).await {
        Ok(id) => Redirect::to(&format!("/runs/{id}")).into_response(),
        Err(e) => {
            let status = e.status;
            match api::blocking(move || Ok(render_home(&s.engine, &f, Some(&e.message)))).await {
                Ok(html) => (status, Html(html)).into_response(),
                Err(e) => html_error(e),
            }
        }
    }
}

#[derive(Debug, Default, Deserialize)]
struct RunPageQuery {
    #[serde(default)]
    error: Option<String>,
}

async fn run_page(State(s): State<AppState>, Path(id): Path<String>, Query(q): Query<RunPageQuery>) -> Response {
    match api::blocking(move || {
        if !api::valid_run_id(&id) {
            return Err(ApiError::not_found("no such run"));
        }
        // Snapshot the event cursor FIRST. A completion racing this render must
        // either be reflected in run.json or have a newer event that reloads it.
        let last_seq = s.engine.history(&id, 0).last().map(|e| e.seq).unwrap_or(0);
        let run = load_run(&s.engine, &id)?;
        let artifacts = api::run_dir(&s.engine, &id)?.list();
        let policy = api::read_artifact(&s.engine, &id, "policy.md").ok();
        let view = RunView {
            run: &run,
            artifacts: &artifacts,
            policy: policy.as_deref(),
            last_seq,
            flash: flash_message(q.error.as_deref()),
        };
        Ok(views::run_page(&view).into_string())
    })
    .await
    {
        Ok(html) => Html(html).into_response(),
        Err(e) => html_error(e),
    }
}

async fn cancel_form(State(s): State<AppState>, Path(id): Path<String>) -> Response {
    back_to_run(&id, api::cancel(&s.engine, &id).err())
}

async fn retry_form(State(s): State<AppState>, Path((id, stage)): Path<(String, String)>) -> Response {
    let retry_id = id.clone();
    back_to_run(&id, api::blocking(move || api::retry(&s.engine, &retry_id, &stage)).await.err())
}

fn flash_message(code: Option<&str>) -> Option<&'static str> {
    match code? {
        "busy" => Some("A run is active or this action is no longer available. Refresh and try again."),
        "not_found" => Some("The requested run could not be found."),
        "invalid" => Some("The action could not be completed. Check the run's saved error and retry prerequisites."),
        "failed" => Some("The action failed. Check the server log for details."),
        _ => None,
    }
}

/// Redirects carry fixed codes, never caller-controlled text displayed as advice.
fn back_to_run(id: &str, err: Option<ApiError>) -> Response {
    if !api::valid_run_id(id) {
        return Redirect::to("/").into_response();
    }
    match err {
        None => Redirect::to(&format!("/runs/{id}")).into_response(),
        Some(e) => {
            eprintln!("run {id} action failed: {}", e.message);
            let code = match e.status {
                StatusCode::CONFLICT => "busy",
                StatusCode::NOT_FOUND => "not_found",
                StatusCode::BAD_REQUEST => "invalid",
                _ => "failed",
            };
            Redirect::to(&format!("/runs/{id}?error={code}")).into_response()
        }
    }
}

fn html_error(e: ApiError) -> Response {
    let body = views::layout(
        "Error",
        maud::html! {
            section.card {
                h2 { (e.status.as_u16()) " " (e.status.canonical_reason().unwrap_or("")) }
                p { (e.message) }
                p { a href="/" { "Back to runs" } }
            }
        },
    );
    (e.status, Html(body.into_string())).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::events::{events_path, EventKind, EventSink, StageId};
    use crate::state::{RunState, Status};
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    fn app() -> (tempfile::TempDir, Arc<Engine>, Router) {
        let home = tempfile::tempdir().unwrap();
        let engine = Engine::new(home.path().to_path_buf(), Config::default());
        let app = router(AppState::new(engine.clone(), "127.0.0.1:7700".parse().unwrap()));
        (home, engine, app)
    }

    fn seed(engine: &Engine, id: &str) {
        let rd = engine.run_dir(id).unwrap();
        rd.create().unwrap();
        let req = RunRequest {
            repo: PathBuf::from("."),
            issue: "#1".into(),
            issue_text: None,
            provider: None,
            policy_acknowledged: false,
        };
        let mut st = RunState::new(id.into(), req, "claude".into());
        st.status = Status::Failed;
        st.save(&rd.root).unwrap();
        rd.write("20-repro/output.txt", "boom").unwrap();
        let sink = EventSink::open(id, &events_path(&rd.root)).unwrap();
        sink.emit(EventKind::StageStarted { stage: StageId::Intake, attempt: 1 });
        sink.info("hello");
    }

    async fn call(app: &Router, method: &str, uri: &str) -> (StatusCode, String) {
        let req = Request::builder()
            .method(method)
            .uri(uri)
            .header(header::HOST, "127.0.0.1:7700")
            .header(header::ORIGIN, "http://127.0.0.1:7700")
            .body(Body::empty())
            .unwrap();
        let res = app.clone().oneshot(req).await.unwrap();
        let status = res.status();
        let bytes = res.into_body().collect().await.unwrap().to_bytes();
        (status, String::from_utf8_lossy(&bytes).into_owned())
    }

    #[tokio::test]
    async fn list_runs_empty_home() {
        let (_h, _e, app) = app();
        let (status, body) = call(&app, "GET", "/api/runs").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body, "[]");
    }

    #[tokio::test]
    async fn unknown_run_is_404() {
        let (_h, _e, app) = app();
        let (status, body) = call(&app, "GET", "/api/runs/nope").await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert!(body.contains("\"error\""), "{body}");
        let (status, _) = call(&app, "GET", "/runs/nope").await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        let (status, _) = call(&app, "GET", "/api/runs/..").await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn run_artifacts_and_pages() {
        let (_h, engine, app) = app();
        seed(&engine, "r1");
        let (status, body) = call(&app, "GET", "/api/runs/r1").await;
        assert_eq!(status, StatusCode::OK);
        assert!(body.contains("\"id\":\"r1\""));
        let (status, body) = call(&app, "GET", "/api/runs/r1/artifacts/20-repro/output.txt").await;
        assert_eq!((status, body.as_str()), (StatusCode::OK, "boom"));
        let (status, _) = call(&app, "GET", "/api/runs/r1/artifacts/..%2Frun.json").await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        let (status, _) = call(&app, "GET", "/runs/r1/artifacts/missing.txt").await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        let (status, body) = call(&app, "GET", "/runs/r1").await;
        assert_eq!(status, StatusCode::OK);
        assert!(body.contains("r1") && body.contains("S6 Draft PR") && body.contains(r#"data-seq="2""#));
        let (status, body) = call(&app, "GET", "/").await;
        assert_eq!(status, StatusCode::OK);
        assert!(body.contains("/runs/r1"));
        let (status, _) = call(&app, "POST", "/api/runs/r1/retry/bogus").await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        let (status, _) = call(&app, "POST", "/api/runs/r1/cancel").await;
        assert_eq!(status, StatusCode::CONFLICT);
        let (status, css) = call(&app, "GET", "/static/app.css").await;
        assert_eq!(status, StatusCode::OK);
        assert!(!css.is_empty());
    }

    #[tokio::test]
    async fn sse_replays_history() {
        let (_h, engine, app) = app();
        seed(&engine, "r1");
        let req = Request::builder()
            .uri("/api/runs/r1/events?since=0")
            .header(header::HOST, "127.0.0.1:7700")
            .body(Body::empty())
            .unwrap();
        let res = app.oneshot(req).await.unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        assert!(res.headers()[header::CONTENT_TYPE].to_str().unwrap().starts_with("text/event-stream"));
        let mut body = res.into_body();
        let mut text = String::new();
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
        while !text.contains("id: 2") {
            let frame = tokio::time::timeout_at(deadline, body.frame()).await.expect("timed out").unwrap().unwrap();
            if let Ok(data) = frame.into_data() {
                text.push_str(&String::from_utf8_lossy(&data));
            }
        }
        assert!(text.contains("event: run"));
        assert!(text.contains("\"kind\":\"stage_started\""));
    }

    #[tokio::test]
    async fn form_errors_rerender_home() {
        let (_h, _e, app) = app();
        let req = Request::builder()
            .method("POST")
            .uri("/runs")
            .header(header::HOST, "127.0.0.1:7700")
            .header(header::ORIGIN, "http://127.0.0.1:7700")
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .body(Body::from("repo=Z%3A%5Cdefinitely%5Cmissing&issue=%2342&provider=claude"))
            .unwrap();
        let res = app.oneshot(req).await.unwrap();
        assert_eq!(res.status(), StatusCode::BAD_REQUEST);
        let body = String::from_utf8_lossy(&res.into_body().collect().await.unwrap().to_bytes()).into_owned();
        assert!(body.contains("is not a directory"), "{body}");
        assert!(body.contains("value=\"#42\""), "form keeps values");
    }

    #[tokio::test]
    async fn localhost_guards_reject_rebinding_and_cross_site_requests_before_handlers() {
        let (_h, engine, app) = app();
        for uri in ["/runs", "/api/runs", "/runs/r1/cancel", "/api/runs/r1/retry/fix", "/api/doctor"] {
            for origin in [None, Some("null"), Some("https://evil.example"), Some("http://localhost:7700")] {
                let mut req = Request::builder().method("POST").uri(uri).header(header::HOST, "127.0.0.1:7700");
                if let Some(origin) = origin {
                    req = req.header(header::ORIGIN, origin);
                }
                let res = app.clone().oneshot(req.body(Body::empty()).unwrap()).await.unwrap();
                assert_eq!(res.status(), StatusCode::FORBIDDEN, "{uri}: {origin:?}");
            }
        }
        assert!(engine.list().is_empty());
        for host in ["evil.example:7700", "127.0.0.1.evil.example:7700", "127.0.0.1:7701", "localhost", "0.0.0.0:7700"]
        {
            let req = Request::builder().uri("/api/config").header(header::HOST, host).body(Body::empty()).unwrap();
            assert_eq!(app.clone().oneshot(req).await.unwrap().status(), StatusCode::FORBIDDEN, "{host}");
        }
        for site in ["cross-site", "same-site"] {
            let req = Request::builder()
                .uri("/api/config")
                .header(header::HOST, "127.0.0.1:7700")
                .header("sec-fetch-site", site)
                .body(Body::empty())
                .unwrap();
            assert_eq!(app.clone().oneshot(req).await.unwrap().status(), StatusCode::FORBIDDEN);
        }
        assert_eq!(
            app.oneshot(Request::builder().uri("/").body(Body::empty()).unwrap()).await.unwrap().status(),
            StatusCode::FORBIDDEN
        );
    }

    #[tokio::test]
    async fn safe_local_requests_work_and_doctor_get_cannot_launch_commands() {
        let (_h, _engine, app) = app();
        for host in ["127.0.0.1:7700", "localhost:7700", "[::1]:7700"] {
            let req = Request::builder().uri("/api/config").header(header::HOST, host).body(Body::empty()).unwrap();
            let res = app.clone().oneshot(req).await.unwrap();
            assert_eq!(res.status(), StatusCode::OK);
            assert_eq!(res.headers()[header::X_CONTENT_TYPE_OPTIONS], "nosniff");
        }
        let (status, _) = call(&app, "GET", "/api/doctor").await;
        assert_eq!(status, StatusCode::METHOD_NOT_ALLOWED);
    }

    #[tokio::test]
    async fn query_flash_cannot_inject_advice() {
        let (_h, engine, app) = app();
        seed(&engine, "r1");
        let (_, html) = call(&app, "GET", "/runs/r1?error=run%20malicious%20command").await;
        assert!(!html.contains("run malicious command"));
        let (_, html) = call(&app, "GET", "/runs/r1?error=busy").await;
        assert!(html.contains("A run is active or this action is no longer available"));
        let response = back_to_run("r1", Some(ApiError::bad_request("attacker text")));
        assert_eq!(response.headers()[header::LOCATION], "/runs/r1?error=invalid");
    }

    #[tokio::test]
    async fn artifact_responses_are_plain_text_and_terminal_sse_closes() {
        let (_h, engine, app) = app();
        seed(&engine, "r1");
        let req = Request::builder()
            .uri("/api/runs/r1/artifacts/20-repro/output.txt")
            .header(header::HOST, "127.0.0.1:7700")
            .body(Body::empty())
            .unwrap();
        let res = app.clone().oneshot(req).await.unwrap();
        assert_eq!(res.headers()[header::X_CONTENT_TYPE_OPTIONS], "nosniff");
        assert_eq!(res.headers()[header::CONTENT_TYPE], "text/plain; charset=utf-8");
        let (_, text) =
            tokio::time::timeout(std::time::Duration::from_secs(2), call(&app, "GET", "/api/runs/r1/events?since=1"))
                .await
                .expect("terminal SSE must end");
        assert!(!text.contains("id: 1\n"));
        assert!(text.contains("id: 2\n"));
        assert!(text.contains("event: end"));
        let req = Request::builder()
            .uri("/api/runs/r1/events?since=0")
            .header(header::HOST, "127.0.0.1:7700")
            .header("last-event-id", "2")
            .body(Body::empty())
            .unwrap();
        let res = app.oneshot(req).await.unwrap();
        let body = tokio::time::timeout(std::time::Duration::from_secs(2), res.into_body().collect())
            .await
            .unwrap()
            .unwrap()
            .to_bytes();
        let text = String::from_utf8_lossy(&body);
        assert!(!text.contains("event: run"));
        assert!(text.contains("event: end"));
    }
}
