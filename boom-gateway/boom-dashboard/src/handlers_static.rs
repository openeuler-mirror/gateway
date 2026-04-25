use axum::http::StatusCode;
use axum::http::header;
use axum::response::{Html, IntoResponse, Response};

const INDEX_HTML: &str = include_str!("frontend/index.html");
const STYLE_CSS: &str = include_str!("frontend/style.css");
const APP_JS: &str = include_str!("frontend/app.js");

/// Redirect `/` to `/dashboard`.
pub async fn redirect_root() -> Response {
    (
        StatusCode::FOUND,
        [(header::LOCATION, "/dashboard")],
    )
        .into_response()
}

pub async fn index() -> Html<&'static str> {
    Html(INDEX_HTML)
}

pub async fn style_css() -> Response {
    (
        [(header::CONTENT_TYPE, "text/css; charset=utf-8")],
        STYLE_CSS,
    )
        .into_response()
}

pub async fn app_js() -> Response {
    (
        [(header::CONTENT_TYPE, "application/javascript; charset=utf-8")],
        APP_JS,
    )
        .into_response()
}

/// SPA fallback: return index.html for any unmatched path under /dashboard/.
pub async fn spa_fallback() -> Html<&'static str> {
    Html(INDEX_HTML)
}
