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
// Format auto-detected from magic bytes — SVG (text "<svg") or PNG (89 50 4E 47).
// To use a PNG: drop your file at frontend/assets/<Vendor>.png — filename
// convention is <Vendor>.<ext> (e.g. GLM.png, MiniMax.png, Qwen.png,
// DeepSeek.svg, Kimi.svg, MiMo.svg, default.svg).
const VENDOR_GLM: &[u8] = include_bytes!("frontend/assets/GLM.png");
const VENDOR_MINIMAX: &[u8] = include_bytes!("frontend/assets/MiniMax.png");
const VENDOR_QWEN: &[u8] = include_bytes!("frontend/assets/Qwen.png");
const VENDOR_DEEPSEEK: &[u8] = include_bytes!("frontend/assets/DeepSeek.svg");
const VENDOR_KIMI: &[u8] = include_bytes!("frontend/assets/Kimi.svg");
const VENDOR_MIMO: &[u8] = include_bytes!("frontend/assets/MiMo.svg");
const VENDOR_DEFAULT: &[u8] = include_bytes!("frontend/assets/default.svg");

/// Auto-detect image content-type from magic bytes.
/// PNG: starts with `89 50 4E 47 0D 0A 1A 0A`
/// SVG: starts with `<?xml` or `<svg` (leading whitespace tolerated)
fn content_type_for(bytes: &[u8]) -> &'static str {
    // PNG magic bytes
    if bytes.len() >= 8 && bytes[..8] == [0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A] {
        return "image/png";
    }
    // SVG: scan first 32 bytes for "<?xml" or "<svg" (skip leading whitespace/BOM)
    let head = bytes.len().min(64);
    let leading = &bytes[..head];
    let trimmed = leading
        .iter()
        .skip_while(|b| matches!(b, b' ' | b'\t' | b'\n' | b'\r' | 0xEF | 0xBB | 0xBF))
        .take(5)
        .copied()
        .collect::<Vec<u8>>();
    if trimmed.starts_with(b"<?xml") || trimmed.starts_with(b"<svg") {
        return "image/svg+xml";
    }
    // Fallback: assume SVG (binary embeds are always SVG or PNG)
    "image/svg+xml"
}

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

/// `/dashboard/assets/vendor/:name` — emits the logo bytes for the matched
/// vendor, falling back to the gray placeholder for unknown names.
/// Content-type is auto-detected from magic bytes (PNG or SVG).
pub async fn vendor_logo(Path(name): Path<String>) -> Response {
    let bytes = match name.as_str() {
        "glm" => VENDOR_GLM,
        "minimax" => VENDOR_MINIMAX,
        "qwen" => VENDOR_QWEN,
        "deepseek" => VENDOR_DEEPSEEK,
        "kimi" => VENDOR_KIMI,
        "mimo" => VENDOR_MIMO,
        _ => VENDOR_DEFAULT,
    };
    let ct = content_type_for(bytes);
    (
        [
            (header::CONTENT_TYPE, ct),
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
