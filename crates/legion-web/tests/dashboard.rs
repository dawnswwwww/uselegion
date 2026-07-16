use axum::body::Body;
use axum::http::{Request, StatusCode};
use tower::ServiceExt;

#[tokio::test]
async fn should_serve_dashboard_html() {
    let app = legion_web::router();

    let response = app
        .oneshot(
            Request::builder()
                .uri("/dashboard")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let content_type = response
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    assert!(content_type.starts_with("text/html"));

    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let html = String::from_utf8(body.to_vec()).unwrap();
    assert!(html.contains("Legion Dashboard"));
    assert!(html.contains("<form id=\"input-area\">"));
}

#[tokio::test]
async fn should_serve_dashboard_js_asset() {
    let app = legion_web::router();

    let response = app
        .oneshot(
            Request::builder()
                .uri("/dashboard/assets/dashboard.js")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let content_type = response
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    assert_eq!(content_type, "application/javascript");

    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let js = String::from_utf8(body.to_vec()).unwrap();
    assert!(js.contains("WebSocket"));
    assert!(js.contains("agent"));
}
