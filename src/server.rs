//! Localhost-only HTTP surface (axum). Binds 127.0.0.1 only (ADR-0002).

use std::net::{IpAddr, SocketAddr};

use axum::extract::Query;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::json;

use crate::api::{self, ProjectAddRequest, ProjectRefBody};
use crate::config::{DEFAULT_SERVE_PORT, loopback_addr, require_loopback};
use crate::error::CoordinatorError;

/// Build the axum router (shared ops via `api`).
pub fn app() -> Router {
    Router::new()
        .route("/health", get(health))
        .route("/v1/projects", get(list_projects).post(add_project))
        .route("/v1/status", get(get_status))
        .route("/v1/run", post(post_run))
        .route("/v1/pause", post(post_pause))
        .route("/v1/resume", post(post_resume))
        .route("/v1/stop", post(post_stop))
}

async fn health() -> impl IntoResponse {
    Json(json!({ "ok": true, "service": "coordinator" }))
}

async fn list_projects() -> Result<impl IntoResponse, ApiError> {
    let projects = api::project_list()?;
    Ok(Json(json!({ "projects": projects })))
}

async fn add_project(Json(body): Json<ProjectAddRequest>) -> Result<impl IntoResponse, ApiError> {
    let rec = api::project_add(std::path::Path::new(&body.path))?;
    Ok((StatusCode::CREATED, Json(rec)))
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
    let view = api::cmd_run(body.project.as_deref(), body.track)?;
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

/// Serve on loopback only.
pub async fn serve(port: u16) -> Result<(), CoordinatorError> {
    require_loopback(crate::config::LOOPBACK)?;
    let addr = loopback_addr(port);
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .map_err(|e| CoordinatorError::Message(format!("bind {addr}: {e}")))?;
    eprintln!("coordinator serve listening on http://{addr}");
    axum::serve(listener, app())
        .with_graceful_shutdown(shutdown_signal())
        .await
        .map_err(|e| CoordinatorError::Message(format!("server error: {e}")))?;
    Ok(())
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
    use crate::config::ENV_COORDINATOR_HOME;
    use http_body_util::BodyExt;
    use std::sync::{Mutex, OnceLock};
    use tempfile::tempdir;
    use tower::ServiceExt;

    fn env_lock() -> &'static Mutex<()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
    }

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
        let _guard = env_lock().lock().unwrap();
        let home = tempdir().unwrap();
        let proj = tempdir().unwrap();
        // SAFETY: serialized by env_lock; restored before guard drop.
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
