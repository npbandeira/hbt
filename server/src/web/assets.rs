//! Arquivos estáticos embutidos no binário.

use std::sync::OnceLock;

use axum::extract::Path;
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};

const CSS: &[u8] = include_bytes!("../../assets/app.css");
const JS: &[u8] = include_bytes!("../../assets/app.js");
const HTMX: &[u8] = include_bytes!("../../assets/htmx.min.js");
const LITERATA: &[u8] = include_bytes!("../../assets/literata.woff2");
const MARTIAN_MONO: &[u8] = include_bytes!("../../assets/martian-mono.woff2");

/// Versão do conteúdo (hash FNV-1a de CSS+JS), usada como `?v=` para o
/// navegador buscar de novo quando os arquivos mudam e poder guardar para sempre
/// enquanto não mudam.
fn version() -> &'static str {
    static V: OnceLock<String> = OnceLock::new();
    V.get_or_init(|| {
        let mut h: u64 = 0xcbf2_9ce4_8422_2325;
        for b in CSS.iter().chain(JS).chain(HTMX) {
            h = (h ^ u64::from(*b)).wrapping_mul(0x0100_0000_01b3);
        }
        format!("{h:016x}")
    })
}

/// URL de um asset com a versão do conteúdo.
pub fn url(file: &str) -> String {
    format!("/assets/{file}?v={}", version())
}

pub async fn serve(Path(file): Path<String>) -> Response {
    let (kind, body) = match file.as_str() {
        "app.css" => ("text/css; charset=utf-8", CSS),
        "app.js" => ("text/javascript; charset=utf-8", JS),
        "htmx.min.js" => ("text/javascript; charset=utf-8", HTMX),
        "literata.woff2" => ("font/woff2", LITERATA),
        "martian-mono.woff2" => ("font/woff2", MARTIAN_MONO),
        _ => return StatusCode::NOT_FOUND.into_response(),
    };
    (
        [
            (header::CONTENT_TYPE, kind),
            (header::CACHE_CONTROL, "public, max-age=31536000, immutable"),
        ],
        body,
    )
        .into_response()
}
