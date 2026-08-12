//! Localhost-only HTTP surface (axum). Binds 127.0.0.1 only (ADR-0002).

use std::net::{IpAddr, SocketAddr};

use axum::extract::Query;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::json;

use crate::api::{
    self, HarnessPromptBody, OutcomeWriteBody, ProjectAddRequest, ProjectRefBody,
    ProjectScanRequest, ProjectSetRequest,
};
use crate::config::{DEFAULT_SERVE_PORT, loopback_addr, require_loopback};
use crate::error::CoordinatorError;

/// Build the axum router (shared ops via `api`).
pub fn app() -> Router {
    Router::new()
        .route("/health", get(health))
        .route("/v1/projects", get(list_projects).post(add_project))
        .route("/v1/projects/set", post(set_project))
        .route("/v1/projects/scan", post(scan_projects))
        .route("/v1/status", get(get_status))
        .route("/v1/run", post(post_run))
        .route("/v1/pause", post(post_pause))
        .route("/v1/resume", post(post_resume))
        .route("/v1/stop", post(post_stop))
        .route("/v1/outcome", get(get_outcome).post(post_outcome))
        .route("/v1/harness/grok/start", post(post_grok_start))
        .route("/v1/harness/grok/prompt", post(post_grok_prompt))
        .route("/v1/harness/grok/compact", post(post_grok_compact))
        .route("/v1/harness/grok/status", get(get_grok_status))
        .route("/v1/harness/grok/shutdown", post(post_grok_shutdown))
}

async fn health() -> impl IntoResponse {
    Json(json!({ "ok": true, "service": "coordinator" }))
}

async fn list_projects() -> Result<impl IntoResponse, ApiError> {
    let projects = api::project_list()?;
    Ok(Json(json!({ "projects": projects })))
}

async fn add_project(Json(body): Json<ProjectAddRequest>) -> Result<impl IntoResponse, ApiError> {
    let rec = api::project_add_request(body)?;
    Ok((StatusCode::CREATED, Json(rec)))
}

async fn set_project(Json(body): Json<ProjectSetRequest>) -> Result<impl IntoResponse, ApiError> {
    let rec = api::project_set_request(body)?;
    Ok(Json(rec))
}

async fn scan_projects(
    Json(body): Json<ProjectScanRequest>,
) -> Result<impl IntoResponse, ApiError> {
    let (candidates, added) = api::project_scan(&body.roots, body.add)?;
    Ok(Json(json!({
        "candidates": candidates,
        "added": added,
        "mode": if body.add { "add" } else { "dry-run" },
    })))
}

#[derive(Debug, Deserialize)]
struct StatusQuery {
    project: Option<String>,
}

async fn get_status(Query(q): Query<StatusQuery>) -> Result<impl IntoResponse, ApiError> {
    if q.project.is_some() {
        let view = api::status(q.project.as_deref())?;
        Ok(Json(json!(view)))
    } else {
        // Single project → object; multiple → array under `projects`
        let all = api::status_all()?;
        if all.len() == 1 {
            Ok(Json(json!(all[0])))
        } else {
            Ok(Json(json!({ "projects": all })))
        }
    }
}

async fn post_run(Json(body): Json<ProjectRefBody>) -> Result<impl IntoResponse, ApiError> {
    let view = api::cmd_run(body.project.as_deref(), body.track, body.driver.as_deref())?;
    Ok(Json(view))
}

async fn post_pause(Json(body): Json<ProjectRefBody>) -> Result<impl IntoResponse, ApiError> {
    let view = api::cmd_pause(body.project.as_deref())?;
    Ok(Json(view))
}

async fn post_resume(Json(body): Json<ProjectRefBody>) -> Result<impl IntoResponse, ApiError> {
    let view = api::cmd_resume(body.project.as_deref())?;
    Ok(Json(view))
}

async fn post_stop(Json(body): Json<ProjectRefBody>) -> Result<impl IntoResponse, ApiError> {
    let view = api::cmd_stop(body.project.as_deref())?;
    Ok(Json(view))
}

async fn post_outcome(Json(body): Json<OutcomeWriteBody>) -> Result<impl IntoResponse, ApiError> {
    let view = api::cmd_outcome_post(body)?;
    Ok(Json(view))
}

async fn post_grok_start(Json(body): Json<ProjectRefBody>) -> Result<impl IntoResponse, ApiError> {
    let view = api::cmd_harness_grok_start(body.project.as_deref(), true).await?;
    Ok(Json(view))
}

async fn post_grok_prompt(
    Json(body): Json<HarnessPromptBody>,
) -> Result<impl IntoResponse, ApiError> {
    let view = api::cmd_harness_grok_prompt_body(body).await?;
    Ok(Json(view))
}

async fn post_grok_compact(
    Json(body): Json<ProjectRefBody>,
) -> Result<impl IntoResponse, ApiError> {
    let view = api::cmd_harness_grok_compact(body.project.as_deref()).await?;
    Ok(Json(view))
}

async fn get_grok_status(Query(q): Query<StatusQuery>) -> Result<impl IntoResponse, ApiError> {
    let view = api::cmd_harness_grok_status(q.project.as_deref()).await?;
    Ok(Json(view))
}

async fn post_grok_shutdown(
    Json(body): Json<ProjectRefBody>,
) -> Result<impl IntoResponse, ApiError> {
    let view = api::cmd_harness_grok_shutdown(body.project.as_deref()).await?;
    Ok(Json(view))
}

async fn get_outcome(Query(q): Query<StatusQuery>) -> Result<impl IntoResponse, ApiError> {
    match api::cmd_outcome_show(q.project.as_deref())? {
        Some(o) => Ok(Json(json!(o)).into_response()),
        None => Ok((
            StatusCode::NOT_FOUND,
            Json(json!({ "error": "no current outcome file" })),
        )
            .into_response()),
    }
}

/// Serve on loopback only; background task polls outcomes + timeouts.
pub async fn serve(port: u16) -> Result<(), CoordinatorError> {
    require_loopback(crate::config::LOOPBACK)?;
    let addr = loopback_addr(port);
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .map_err(|e| CoordinatorError::Message(format!("bind {addr}: {e}")))?;
    eprintln!("coordinator serve listening on http://{addr}");

    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
    let poll_handle = tokio::spawn(crate::watch::serve_poll_loop(shutdown_rx));

    let result = axum::serve(listener, app())
        .with_graceful_shutdown(async move {
            shutdown_signal().await;
            let _ = shutdown_tx.send(true);
        })
        .await
        .map_err(|e| CoordinatorError::Message(format!("server error: {e}")));

    let _ = poll_handle.await;
    result
}

/// Explicit bind with IP validation (for tests / future config).
pub fn validated_bind_addr(ip: IpAddr, port: u16) -> Result<SocketAddr, CoordinatorError> {
    require_loopback(ip)?;
    Ok(SocketAddr::new(ip, port))
}

pub fn default_port() -> u16 {
    DEFAULT_SERVE_PORT
}

async fn shutdown_signal() {
    let ctrl_c = async {
        if let Err(e) = tokio::signal::ctrl_c().await {
            eprintln!("failed to install Ctrl+C handler: {e}");
        }
    };

    #[cfg(windows)]
    {
        ctrl_c.await;
    }

    #[cfg(not(windows))]
    {
        let terminate = async {
            match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
                Ok(mut s) => {
                    s.recv().await;
                }
                Err(_) => std::future::pending::<()>().await,
            }
        };
        tokio::select! {
            _ = ctrl_c => {},
            _ = terminate => {},
        }
    }
}

struct ApiError(CoordinatorError);

impl From<CoordinatorError> for ApiError {
    fn from(value: CoordinatorError) -> Self {
        Self(value)
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let status =
            StatusCode::from_u16(self.0.http_status()).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
        let body = Json(json!({
            "error": self.0.to_string(),
        }));
        (status, body).into_response()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{ENV_COORDINATOR_HOME, test_env_lock};
    use http_body_util::BodyExt;
    use tempfile::tempdir;
    use tower::ServiceExt;

    #[tokio::test]
    async fn health_ok() {
        let response = app()
            .oneshot(
                axum::http::Request::builder()
                    .uri("/health")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(v["ok"], true);
    }

    /// Holds std Mutex across awaits to serialize process-wide COORDINATOR_HOME.
    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn projects_add_and_status() {
        let _guard = test_env_lock();
        let home = tempdir().unwrap();
        let proj = tempdir().unwrap();
        // SAFETY: serialized by test_env_lock; restored before guard drop.
        unsafe {
            std::env::set_var(ENV_COORDINATOR_HOME, home.path());
        }

        let body = serde_json::to_vec(&json!({ "path": proj.path().to_string_lossy() })).unwrap();
        let response = app()
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri("/v1/projects")
                    .header("content-type", "application/json")
                    .body(axum::body::Body::from(body))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::CREATED);

        let run_body = serde_json::to_vec(&json!({})).unwrap();
        let response = app()
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri("/v1/run")
                    .header("content-type", "application/json")
                    .body(axum::body::Body::from(run_body))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(v["status"], "Running");
        assert!(v.get("project_id").is_some());
        assert!(v.get("path").is_some());
        assert!(v.get("phase").is_some());
        assert!(v.get("last_event").is_some());
        assert!(v.get("run_epoch").is_some());

        unsafe {
            std::env::remove_var(ENV_COORDINATOR_HOME);
        }
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn projects_set_and_scan() {
        let _guard = test_env_lock();
        let home = tempdir().unwrap();
        let scan_root = tempdir().unwrap();
        let proj = scan_root.path().join("ScanMe");
        std::fs::create_dir_all(proj.join("conductor")).unwrap();
        std::fs::write(proj.join("conductor").join("conductor.md"), "# t\n").unwrap();
        unsafe {
            std::env::set_var(ENV_COORDINATOR_HOME, home.path());
        }

        // Dry-run scan
        let body = serde_json::to_vec(&json!({
            "roots": [scan_root.path().to_string_lossy()],
            "add": false
        }))
        .unwrap();
        let response = app()
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri("/v1/projects/scan")
                    .header("content-type", "application/json")
                    .body(axum::body::Body::from(body))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(v["mode"], "dry-run");
        assert_eq!(v["candidates"].as_array().unwrap().len(), 1);

        // Add via scan
        let body = serde_json::to_vec(&json!({
            "roots": [scan_root.path().to_string_lossy()],
            "add": true
        }))
        .unwrap();
        let response = app()
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri("/v1/projects/scan")
                    .header("content-type", "application/json")
                    .body(axum::body::Body::from(body))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(v["added"].as_array().unwrap().len(), 1);

        // Second add is idempotent (no duplicate)
        let body = serde_json::to_vec(&json!({
            "roots": [scan_root.path().to_string_lossy()],
            "add": true
        }))
        .unwrap();
        let response = app()
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri("/v1/projects/scan")
                    .header("content-type", "application/json")
                    .body(axum::body::Body::from(body))
                    .unwrap(),
            )
            .await
            .unwrap();
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(v["added"].as_array().unwrap().len(), 0);
        assert!(v["candidates"][0]["already_registered"].as_bool().unwrap());

        // Set profile
        let body = serde_json::to_vec(&json!({
            "layout_profile": "single_root"
        }))
        .unwrap();
        let response = app()
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri("/v1/projects/set")
                    .header("content-type", "application/json")
                    .body(axum::body::Body::from(body))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(v["layout_profile"], "single_root");

        // Status always includes execution_repo key (may be string or null)
        let response = app()
            .oneshot(
                axum::http::Request::builder()
                    .uri("/v1/status")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert!(v.get("layout_profile").is_some());
        assert!(v.as_object().unwrap().contains_key("execution_repo"));
        assert!(v.as_object().unwrap().contains_key("conductor_dir"));

        unsafe {
            std::env::remove_var(ENV_COORDINATOR_HOME);
        }
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn post_outcome_success() {
        let _guard = test_env_lock();
        let home = tempdir().unwrap();
        let proj = tempdir().unwrap();
        unsafe {
            std::env::set_var(ENV_COORDINATOR_HOME, home.path());
        }

        let body = serde_json::to_vec(&json!({ "path": proj.path().to_string_lossy() })).unwrap();
        let response = app()
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri("/v1/projects")
                    .header("content-type", "application/json")
                    .body(axum::body::Body::from(body))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::CREATED);

        let run_body = serde_json::to_vec(&json!({})).unwrap();
        let _ = app()
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri("/v1/run")
                    .header("content-type", "application/json")
                    .body(axum::body::Body::from(run_body))
                    .unwrap(),
            )
            .await
            .unwrap();

        let outcome_body = serde_json::to_vec(&json!({
            "phase": "plan",
            "status": "success",
            "source": "http",
            "message": "via api"
        }))
        .unwrap();
        let response = app()
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri("/v1/outcome")
                    .header("content-type", "application/json")
                    .body(axum::body::Body::from(outcome_body))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(v["status"], "Running");
        assert_eq!(v["phase"], "plan-review");

        unsafe {
            std::env::remove_var(ENV_COORDINATOR_HOME);
        }
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn harness_start_missing_binary_is_400() {
        use crate::harness::ENV_GROK_BIN;

        let _guard = test_env_lock();
        let home = tempdir().unwrap();
        let proj = tempdir().unwrap();
        unsafe {
            std::env::set_var(ENV_COORDINATOR_HOME, home.path());
            std::env::set_var(ENV_GROK_BIN, r"C:\this\does\not\exist-grok.exe");
        }

        let body = serde_json::to_vec(&json!({ "path": proj.path().to_string_lossy() })).unwrap();
        let response = app()
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri("/v1/projects")
                    .header("content-type", "application/json")
                    .body(axum::body::Body::from(body))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::CREATED);

        let start_body = serde_json::to_vec(&json!({})).unwrap();
        let response = app()
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri("/v1/harness/grok/start")
                    .header("content-type", "application/json")
                    .body(axum::body::Body::from(start_body))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert!(
            v["error"].as_str().unwrap_or("").contains("not found"),
            "error={}",
            v["error"]
        );

        unsafe {
            std::env::remove_var(ENV_COORDINATOR_HOME);
            std::env::remove_var(ENV_GROK_BIN);
        }
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn harness_prompt_and_status_via_http() {
        use crate::harness::grok::{
            GrokSession, mock_handshake_ok, rpc_result, session_update_chunk,
        };
        use crate::harness::pool::insert_test_session;
        use std::time::Duration;

        let _guard = test_env_lock();
        let home = tempdir().unwrap();
        let proj = tempdir().unwrap();
        unsafe {
            std::env::set_var(ENV_COORDINATOR_HOME, home.path());
        }

        let body = serde_json::to_vec(&json!({ "path": proj.path().to_string_lossy() })).unwrap();
        let response = app()
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri("/v1/projects")
                    .header("content-type", "application/json")
                    .body(axum::body::Body::from(body))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::CREATED);
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        let rec: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        let project_id = rec["id"].as_str().unwrap().to_string();

        let run_body = serde_json::to_vec(&json!({})).unwrap();
        let _ = app()
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri("/v1/run")
                    .header("content-type", "application/json")
                    .body(axum::body::Body::from(run_body))
                    .unwrap(),
            )
            .await
            .unwrap();

        let mut lines = mock_handshake_ok("sess-http");
        lines.push(session_update_chunk("pong"));
        lines.push(rpc_result(4, json!({ "stopReason": "end_turn" })));
        let session =
            GrokSession::start_mock(proj.path().to_path_buf(), lines, Duration::from_secs(2))
                .await
                .unwrap();
        insert_test_session(project_id.clone(), session).await;

        let prompt_body = serde_json::to_vec(&json!({ "text": "hi" })).unwrap();
        let response = app()
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri("/v1/harness/grok/prompt")
                    .header("content-type", "application/json")
                    .body(axum::body::Body::from(prompt_body))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(v["text"], "pong");
        assert_eq!(v["applied"], true);

        let response = app()
            .oneshot(
                axum::http::Request::builder()
                    .uri("/v1/status")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert!(v.get("harness").is_some());
        assert_eq!(v["harness"]["grok"]["session_id"], "sess-http");

        let _ = app()
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri("/v1/harness/grok/shutdown")
                    .header("content-type", "application/json")
                    .body(axum::body::Body::from(
                        serde_json::to_vec(&json!({})).unwrap(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();

        unsafe {
            std::env::remove_var(ENV_COORDINATOR_HOME);
        }
    }

    #[test]
    fn validated_bind_rejects_public() {
        let err = validated_bind_addr(IpAddr::V4(std::net::Ipv4Addr::new(192, 168, 1, 1)), 7420)
            .unwrap_err();
        assert!(matches!(err, CoordinatorError::NonLoopbackBind(_)));
    }

    #[test]
    fn validated_bind_accepts_loopback() {
        let addr = validated_bind_addr(IpAddr::V4(std::net::Ipv4Addr::LOCALHOST), 7420).unwrap();
        assert!(addr.ip().is_loopback());
    }
}
