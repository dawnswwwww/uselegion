use crate::websocket::GatewayState;
use axum::body::Bytes;
use axum::extract::{Extension, Path};
use axum::http::{HeaderMap, StatusCode};
use axum::response::Html;
use axum::response::Json;
use legion_automation::task_runner::EnqueueRequest;
use legion_automation::tasks::TaskKind;
use serde_json::{Value, json};
use std::sync::Arc;

/// Serves the bundled Web Dashboard page.
pub async fn dashboard() -> Html<&'static str> {
    legion_web::dashboard_html().await
}

/// Placeholder for the Canvas API surface.
pub async fn canvas_placeholder() -> &'static str {
    "Canvas API placeholder"
}

/// HMAC-verified webhook trigger for cron jobs (automation-advanced Phase C).
///
/// `POST /webhook/{id}` runs the cron job `{id}` when the request carries a
/// valid `X-Hub-Signature-256: sha256=<hmac>` header over the raw body, keyed
/// by the job's `webhook_secret`. Jobs without a secret answer 404 so the
/// endpoint does not reveal which job ids exist.
pub async fn webhook_handler(
    Extension(state): Extension<Arc<GatewayState>>,
    Path(job_id): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> (StatusCode, Json<Value>) {
    let Some(scheduler) = state.cron_scheduler.as_ref() else {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": "cron scheduler not available" })),
        );
    };
    let job = match scheduler.get_job(&job_id).await {
        Ok(Some(job)) => job,
        Ok(None) => {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({ "error": "job not found" })),
            );
        }
        Err(err) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": err.to_string() })),
            );
        }
    };
    let Some(secret) = job.webhook_secret.as_deref() else {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": "job not found" })),
        );
    };
    let signature = headers
        .get("x-hub-signature-256")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default();
    if !legion_automation::cron::verify_webhook_signature(secret, &body, signature) {
        return (
            StatusCode::UNAUTHORIZED,
            Json(json!({ "error": "invalid signature" })),
        );
    }
    let Some(task_runner) = state.task_runner.as_ref() else {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": "task runner not available" })),
        );
    };

    tracing::info!(job_id = %job_id, "webhook triggered cron job; enqueuing background task");
    match task_runner
        .enqueue(EnqueueRequest {
            agent_id: job.agent_id,
            message: job.message,
            kind: TaskKind::Cron,
            depends_on: Vec::new(),
        })
        .await
    {
        Ok(task) => (
            StatusCode::ACCEPTED,
            Json(json!({ "task_id": task.id, "status": "enqueued" })),
        ),
        Err(err) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": err.to_string() })),
        ),
    }
}
