use axum::extract::Path;
use axum::http::StatusCode;
use axum::http::header;
use axum::response::{Html, IntoResponse, Response};

const INDEX_HTML: &str = include_str!("frontend/index.html");
const STYLE_CSS: &str = include_str!("frontend/style.css");
const APP_JS: &str = include_str!("frontend/app.js");
const I18N_JS: &str = include_str!("frontend/i18n.js");

// Vendor logo bytes — placeholder SVGs ship in the binary so a fresh build
// always renders gracefully. Replace the files under frontend/assets/ to
// swap in real logos (same path, same format → no code change required).
const VENDOR_GLM: &[u8] = include_bytes!("frontend/assets/vendor-glm.svg");
const VENDOR_MINIMAX: &[u8] = include_bytes!("frontend/assets/vendor-minimax.svg");
const VENDOR_QWEN: &[u8] = include_bytes!("frontend/assets/vendor-qwen.svg");
const VENDOR_DEFAULT: &[u8] = include_bytes!("frontend/assets/vendor-default.svg");

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

pub async fn i18n_js() -> Response {
    (
        [(header::CONTENT_TYPE, "application/javascript; charset=utf-8")],
        I18N_JS,
    )
        .into_response()
}

/// `/dashboard/assets/vendor/:name` — emits the SVG bytes for the matched
/// vendor, falling back to the gray placeholder for unknown names.
pub async fn vendor_logo(Path(name): Path<String>) -> Response {
    let bytes = match name.as_str() {
        "glm" => VENDOR_GLM,
        "minimax" => VENDOR_MINIMAX,
        "qwen" => VENDOR_QWEN,
        _ => VENDOR_DEFAULT,
    };
    (
        [
            (header::CONTENT_TYPE, "image/svg+xml"),
            (header::CACHE_CONTROL, "public, max-age=3600"),
        ],
        bytes,
    )
        .into_response()
}

/// SPA fallback: return index.html for any unmatched path under /dashboard/.
pub async fn spa_fallback() -> Html<&'static str> {
    Html(INDEX_HTML)
}
