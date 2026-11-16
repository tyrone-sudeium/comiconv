//! Web server for comiconv.
//!
//! Accepts `.zip`/`.cbz` uploads (up to 1 GiB) and returns a comic archive
//! with every image re-encoded as AVIF.
//!
//! The AVIF quality can be chosen per request (`20`, `33`, `50`, `60`) or left
//! on `auto` (the default), which picks it from the upload size: > 200 MiB
//! uses 33, > 50 MiB uses 50, anything smaller uses 60.
//!
//! Endpoints:
//! - `GET /`         simple upload page with a progress bar and quality picker
//! - `POST /convert` raw binary body plus optional `?quality=auto|<0-100>`, e.g.
//!   `curl --data-binary @comic.zip -o comic.cbz http://localhost:8080/convert`
//! - `POST /upload`  multipart/form-data with a field named `file` and an
//!   optional `quality` field

use axum::{
    body::{Body, Bytes},
    extract::{DefaultBodyLimit, Multipart, State},
    extract::multipart::MultipartRejection,
    extract::rejection::{BytesRejection, FailedToBufferBody},
    http::{header, HeaderMap, StatusCode, Uri},
    response::{Html, IntoResponse, Response},
    routing::{get, post},
    Router,
};
use comiconv::{Converter, Format};
use percent_encoding::{utf8_percent_encode, NON_ALPHANUMERIC};
use std::sync::Arc;
use tokio::sync::Semaphore;

/// Maximum size of an uploaded archive (1 GiB)
pub const MAX_FILE_SIZE: usize = 1024 * 1024 * 1024;
const MB: usize = 1024 * 1024;
/// How many archives may be converted at the same time
const MAX_CONCURRENT: usize = 4;

#[derive(Clone)]
struct AppState {
    slots: Arc<Semaphore>,
}

pub fn router() -> Router {
    let state = AppState {
        slots: Arc::new(Semaphore::new(MAX_CONCURRENT)),
    };
    Router::new()
        .route("/", get(index))
        .route("/convert", post(convert_raw))
        .route("/upload", post(convert_multipart))
        // multipart framing adds a little overhead on top of the file itself
        .layer(DefaultBodyLimit::max(MAX_FILE_SIZE + 1024 * 1024))
        .with_state(state)
}

#[tokio::main]
async fn main() {
    let addr = std::env::args().nth(1).unwrap_or_else(|| "127.0.0.1:8080".to_string());
    let listener = tokio::net::TcpListener::bind(&addr)
        .await
        .unwrap_or_else(|e| panic!("failed to bind {addr}: {e}"));
    println!("comiconv-web listening on http://{addr}");
    axum::serve(listener, router())
        .await
        .expect("server error");
}

async fn index() -> Html<&'static str> {
    Html(PAGE)
}

/// Raw binary upload (`application/octet-stream` or similar).
/// Accepts an optional `?quality=auto|<0-100>` query parameter.
async fn convert_raw(
    State(state): State<AppState>,
    headers: HeaderMap,
    uri: Uri,
    body: Result<Bytes, BytesRejection>,
) -> Response {
    let data = match body {
        Ok(data) => data,
        Err(rejection) => return reject_body(rejection),
    };
    let quality = match uri.query().and_then(query_quality).map(parse_quality) {
        Some(Ok(quality)) => quality,
        Some(Err(e)) => return bad_request(e),
        None => None,
    };
    let filename = filename_from_content_disposition(&headers);
    convert(data, filename, state, quality).await
}

/// Multipart upload with a field named `file` (any other single field is
/// accepted too) and an optional `quality` field.
async fn convert_multipart(
    State(state): State<AppState>,
    multipart: Result<Multipart, MultipartRejection>,
) -> Response {
    let Ok(mut multipart) = multipart else {
        return reject(multipart.unwrap_err());
    };
    let mut file: Option<(Option<String>, Bytes)> = None;
    let mut quality_field: Option<String> = None;
    while let Some(field) = multipart.next_field().await.ok().flatten() {
        if field.name().as_deref() == Some("quality") {
            match field.bytes().await {
                Ok(data) => quality_field = Some(String::from_utf8_lossy(&data).into_owned()),
                Err(e) => return bad_request(e.to_string()),
            }
        } else if field.name().as_deref() == Some("file") || file.is_none() {
            let file_name = field.file_name().map(str::to_string);
            match field.bytes().await {
                Ok(data) => file = Some((file_name, data)),
                Err(e) => return bad_request(e.to_string()),
            }
        }
    }
    let quality = match quality_field {
        Some(value) => match parse_quality(&value) {
            Ok(quality) => quality,
            Err(e) => return bad_request(e),
        },
        None => None,
    };
    let Some((filename, data)) = file else {
        return bad_request("expected a form field named 'file'");
    };
    convert(data, filename, state, quality).await
}

/// Validates the upload and converts it to AVIF, answering with a `.cbz`.
/// A `quality` of `None` ("auto") is resolved from the upload size.
async fn convert(
    data: Bytes,
    filename: Option<String>,
    state: AppState,
    quality: Option<u8>,
) -> Response {
    if data.len() > MAX_FILE_SIZE {
        return too_large();
    }
    if !is_zip(&data) {
        return bad_request("not a valid .zip/.cbz archive");
    }
    // conversions are CPU bound (rayon), keep them off the async runtime and
    // limit how many run at once so 1 GiB uploads don't exhaust memory
    let slot = state.slots.acquire_owned().await.expect("semaphore closed");
    let quality = quality.unwrap_or_else(|| auto_quality(data.len()));
    let converter = Converter {
        format: Format::Avif,
        quiet: true,
        speed: 9,
        quality,
        ..Default::default()
    };
    let converted = tokio::task::spawn_blocking(move || converter.convert(&data, None)).await;
    drop(slot);
    match converted {
        Ok(Ok(out)) => {
            let raw_filename = output_name(filename.as_deref());
            let encoded_filename = utf8_percent_encode(&raw_filename, &NON_ALPHANUMERIC);
            let mut headers = HeaderMap::new();
            headers.insert(header::CONTENT_TYPE, "application/x-cbz".parse().unwrap());
            headers.insert(
                header::CONTENT_DISPOSITION,
                format!("attachment; filename=\"{}\"", encoded_filename)
                    .parse()
                    .unwrap(),
            );
            let mut res = Response::new(Body::from(out));
            *res.headers_mut() = headers;
            res
        }
        Ok(Err(e)) => internal(format!("conversion failed: {e}")),
        Err(e) => internal(format!("conversion task failed: {e}")),
    }
}

/// Maps raw-body rejections onto the appropriate status code.
fn reject_body(rejection: BytesRejection) -> Response {
    if matches!(
        rejection,
        BytesRejection::FailedToBufferBody(FailedToBufferBody::LengthLimitError(_))
    ) {
        too_large()
    } else {
        rejection.into_response()
    }
}

fn reject<R: IntoResponse>(rejection: R) -> Response {
    rejection.into_response()
}

fn too_large() -> Response {
    (StatusCode::PAYLOAD_TOO_LARGE, format!("file too large (max {} bytes)", MAX_FILE_SIZE)).into_response()
}

fn bad_request(msg: impl Into<String>) -> Response {
    (StatusCode::BAD_REQUEST, msg.into()).into_response()
}

fn internal(msg: impl Into<String>) -> Response {
    (StatusCode::INTERNAL_SERVER_ERROR, msg.into()).into_response()
}

/// Accepts the local file headers of a normal, empty or spanned zip.
fn is_zip(data: &[u8]) -> bool {
    data.len() >= 4 && data[..2] == *b"PK" && matches!(&data[2..4], [3, 4] | [5, 6] | [7, 8])
}

/// Best effort extraction of a filename from `Content-Disposition`.
fn filename_from_content_disposition(headers: &HeaderMap) -> Option<String> {
    let cd = headers.get(header::CONTENT_DISPOSITION)?.to_str().ok()?;
    cd.split(';')
        .skip(1)
        .find_map(|part| part.trim().strip_prefix("filename="))
        .map(|f| f.trim_matches('"').trim().to_string())
        .filter(|f| !f.is_empty())
}

/// Picks an AVIF quality automatically based on the size of the upload.
fn auto_quality(size: usize) -> u8 {
    if size > 200 * MB {
        33 // Low
    } else if size > 50 * MB {
        50 // High
    } else {
        60 // Very High
    }
}

/// Parses a user-supplied quality: `auto` (or empty) selects automatically,
/// anything else must be a number in 0-100.
fn parse_quality(value: &str) -> Result<Option<u8>, String> {
    let value = value.trim();
    if value.is_empty() || value.eq_ignore_ascii_case("auto") {
        return Ok(None);
    }
    value
        .parse::<u8>()
        .ok()
        .filter(|q| *q <= 100)
        .map(Some)
        .ok_or_else(|| format!("invalid quality '{value}', expected 'auto' or a number 0-100"))
}

/// Finds the value of the `quality` query parameter, if present.
fn query_quality(query: &str) -> Option<&str> {
    query.split('&').find_map(|pair| {
        let (key, val) = pair.split_once('=')?;
        (key == "quality").then_some(val)
    })
}

fn output_name(filename: Option<&str>) -> String {
    let stem = filename
        .map(|f| f.rsplit(['/', '\\']).next().unwrap_or(f))
        .and_then(|base| base.rsplit_once('.').map(|(stem, _)| stem))
        .filter(|s| !s.is_empty())
        .unwrap_or("converted");

    let sanitized: String = stem
        .chars()
        .filter(|c| {
            !c.is_control() && !matches!(*c, '/' | '\\' | '\0' | '"')
        })
        .collect();

    let sanitized = if sanitized.trim().is_empty() {
        "converted"
    } else {
        &sanitized
    };

    // No idea why but rust-analyzer specifically gets very mad without using `return format!(...); here`
    return format!("{}.cbz", sanitized.trim());
}

const PAGE: &str = r#"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<meta name="color-scheme" content="light dark">
<title>comiconv</title>
<style>
    :root {
        --bg: #ffffff;
        --text: #222222;
        --primary: #2563eb;
        --primary-text: #ffffff;
        --disabled-bg: #9ca3af;
        --disabled-text: #ffffff;
        --input-bg: #ffffff;
        --input-border: #cccccc;
        --bar-bg: #e5e7eb;
    }

    @media (prefers-color-scheme: dark) {
        :root {
            --bg: #121212;
            --text: #e5e7eb;
            --primary: #3b82f6;
            --primary-text: #ffffff;
            --disabled-bg: #374151;
            --disabled-text: #9ca3af;
            --input-bg: #1f2937;
            --input-border: #374151;
            --bar-bg: #374151;
        }
    }

    body {
        font-family: system-ui, sans-serif;
        max-width: 40rem;
        margin: 3rem auto;
        padding: 0 1rem;
        background: var(--bg);
        color: var(--text);
    }

    h1 {
        font-size: 1.6rem;
    }

    .column {
        display: flex;
        flex-direction: column;
        align-items: left;
        gap: .5rem;
        margin: 1rem 0;
    }

    .row {
        display: flex;
        gap: 0.5rem;
        margin: 1rem 0;
    }

    button {
        padding: .4rem 1rem;
        font-size: 1rem;
        border: 0;
        border-radius: .3rem;
        background: var(--primary);
        color: var(--primary-text);
        cursor: pointer;
    }

    button:disabled {
        background: var(--disabled-bg);
        color: var(--disabled-text);
        cursor: default;
    }

    select {
        padding: .4rem .5rem;
        font-size: 1rem;
        border: 1px solid var(--input-border);
        border-radius: .3rem;
        background: var(--input-bg);
        color: var(--text);
    }

    #bar {
        height: .5rem;
        background: var(--bar-bg);
        border-radius: .25rem;
        overflow: hidden;
        margin-bottom: .5rem;
    }

    #fill {
        height: 100%;
        width: 0;
        background: var(--primary);
        transition: width .1s;
    }

    #quality {
        flex: 1;
    }
</style>
</head>
<body>
<h1>comiconv</h1>
<p>Upload a <code>.zip</code>/<code>.cbz</code> (max 1&nbsp;GiB) and download the same comic with all images re-encoded as AVIF.</p>
<div class="column">
    <input type="file" id="file" accept=".zip,.cbz,application/zip,application/x-zip-compressed">
    <div class="row">
        <p>Quality:</p>
        <select id="quality" title="AVIF quality (Auto picks from the file size)">
            <option value="auto" selected>Auto</option>
            <option value="20">Very Low (20)</option>
            <option value="33">Low (33)</option>
            <option value="50">High (50)</option>
            <option value="60">Very High (60)</option>
        </select>
    </div>
    <button id="go" disabled>Convert</button>
</div>
<div id="bar"><div id="fill"></div></div>
<p id="status">&nbsp;</p>
<script>
const file = document.getElementById('file');
const go = document.getElementById('go');
const status = document.getElementById('status');
const fill = document.getElementById('fill');

file.onchange = () => { go.disabled = !file.files.length; };
go.onclick = () => {
    if (!file.files.length) return;
    const fd = new FormData();
    fd.append('file', file.files[0]);
    fd.append('quality', document.getElementById('quality').value);
    go.disabled = true;
    status.textContent = 'Uploading\u2026';
    fill.style.width = '0%';
    const xhr = new XMLHttpRequest();
    xhr.open('POST', '/upload');
    xhr.responseType = 'blob';
    xhr.upload.onprogress = (e) => {
        if (e.lengthComputable) {
            const pct = Math.round(e.loaded / e.total * 100);
            fill.style.width = pct + '%';
            status.textContent = 'Uploading\u2026 ' + pct + '%';
        }
    };
    xhr.onload = () => {
        go.disabled = false;
        if (xhr.status === 200) {
            let name = 'converted.cbz';
            const m = (xhr.getResponseHeader('Content-Disposition') || '').match(/filename="([^"]+)"/);
            if (m) {
                try {
                    name = decodeURIComponent(m[1]);
                } catch (e) {
                    name = m[1];
                }
            }
            const a = document.createElement('a');
            a.href = URL.createObjectURL(new Blob([xhr.response], { type: 'application/x-cbz' }));
            a.download = name;
            a.click();
            status.textContent = 'Done! Download started.';
        } else {
            status.textContent = 'Error: ' + (xhr.responseText || xhr.status);
        }
    };
    xhr.onerror = () => { go.disabled = false; status.textContent = 'Upload failed'; };
    xhr.send(fd);
};
</script>
</body>
</html>"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zip_magic() {
        assert!(is_zip(b"PK\x03\x04rest"));
        assert!(is_zip(b"PK\x05\x06rest"));
        assert!(!is_zip(b"PK\x01\x02rest"));
        assert!(!is_zip(b"hello world"));
        assert!(!is_zip(b"PK"));
    }

    #[test]
    fn auto_quality_thresholds() {
        assert_eq!(auto_quality(0), 60);
        // exactly 50 MiB is not "greater than 50 MiB"
        assert_eq!(auto_quality(50 * MB), 60);
        assert_eq!(auto_quality(50 * MB + 1), 50);
        assert_eq!(auto_quality(200 * MB), 50);
        assert_eq!(auto_quality(200 * MB + 1), 33);
    }

    #[test]
    fn parse_quality_values() {
        assert_eq!(parse_quality("auto"), Ok(None));
        assert_eq!(parse_quality("AUTO"), Ok(None));
        assert_eq!(parse_quality(""), Ok(None));
        assert_eq!(parse_quality(" 20 "), Ok(Some(20)));
        assert_eq!(parse_quality("100"), Ok(Some(100)));
        assert!(parse_quality("101").is_err());
        assert!(parse_quality("abc").is_err());
    }

    #[test]
    fn query_quality_lookup() {
        assert_eq!(query_quality("quality=auto"), Some("auto"));
        assert_eq!(query_quality("x=1&quality=33&y=2"), Some("33"));
        assert_eq!(query_quality("x=1"), None);
        assert_eq!(query_quality(""), None);
    }

    #[test]
    fn names() {
        assert_eq!(output_name(None), "converted.cbz");
        assert_eq!(output_name(Some("comic.zip")), "comic.cbz");
        assert_eq!(output_name(Some("/tmp/my comic (1).cbz")), "my comic (1).cbz");
        assert_eq!(output_name(Some("a/b/c.tar.zip")), "c.tar.cbz");
        assert_eq!(output_name(Some("weird<name>.zip")), "weird<name>.cbz");
        assert_eq!(output_name(Some("weird<name>.zip")), "weird<name>.cbz");
        assert_eq!(output_name(Some("[This] {Is A Test}! ♡日本語.cbz")), "[This] {Is A Test}! ♡日本語.cbz");
    }

    #[test]
    fn content_disposition_filename() {
        let mut headers = HeaderMap::new();
        headers.insert(
            header::CONTENT_DISPOSITION,
            "form-data; name=\"file\"; filename=\"comic.zip\"".parse().unwrap(),
        );
        assert_eq!(filename_from_content_disposition(&headers).as_deref(), Some("comic.zip"));
        let empty = HeaderMap::new();
        assert_eq!(filename_from_content_disposition(&empty), None);
    }
}
