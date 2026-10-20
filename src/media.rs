//! `GET /media/...` from MEDIA_ROOT, including byte ranges (video 206s).

use std::io::SeekFrom;
use std::path::PathBuf;

use axum::body::Body;
use axum::extract::{Path, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use tokio::fs::File;
use tokio::io::{AsyncReadExt, AsyncSeekExt};

use crate::state::AppState;

pub async fn serve_media(
    State(state): State<AppState>,
    Path(rel): Path<String>,
    headers: HeaderMap,
) -> Response {
    let rel = rel.trim_start_matches('/');
    if rel.is_empty() || rel.split('/').any(|p| p == ".." || p.is_empty()) {
        return StatusCode::NOT_FOUND.into_response();
    }
    let root = PathBuf::from(&state.config.media_root);
    let path = root.join(rel);
    let Ok(meta) = tokio::fs::metadata(&path).await else {
        return StatusCode::NOT_FOUND.into_response();
    };
    if !meta.is_file() {
        return StatusCode::NOT_FOUND.into_response();
    }
    let len = meta.len();
    let mime = mime_of(rel);
    let range = headers.get(header::RANGE).and_then(|v| v.to_str().ok());
    if let Some(spec) = range.and_then(|r| parse_range(r, len)) {
        let (start, end) = spec;
        let Ok(mut file) = File::open(&path).await else {
            return StatusCode::NOT_FOUND.into_response();
        };
        if file.seek(SeekFrom::Start(start)).await.is_err() {
            return StatusCode::RANGE_NOT_SATISFIABLE.into_response();
        }
        let take = (end - start + 1) as usize;
        let mut buf = vec![0u8; take];
        match file.read_exact(&mut buf).await {
            Ok(_) => {}
            Err(_) => {
                // Short read at EOF: return what we can.
                let _ = file.read_to_end(&mut buf).await;
            }
        }
        let count = (end - start + 1).min(len);
        Response::builder()
            .status(StatusCode::PARTIAL_CONTENT)
            .header(header::CONTENT_TYPE, mime)
            .header(header::ACCEPT_RANGES, "bytes")
            .header(header::CONTENT_RANGE, format!("bytes {start}-{end}/{len}"))
            .header(header::CONTENT_LENGTH, count.to_string())
            .body(Body::from(buf))
            .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())
    } else if range.is_some() {
        Response::builder()
            .status(StatusCode::RANGE_NOT_SATISFIABLE)
            .header(header::CONTENT_RANGE, format!("bytes */{len}"))
            .body(Body::empty())
            .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())
    } else {
        let Ok(bytes) = tokio::fs::read(&path).await else {
            return StatusCode::NOT_FOUND.into_response();
        };
        Response::builder()
            .status(StatusCode::OK)
            .header(header::CONTENT_TYPE, mime)
            .header(header::ACCEPT_RANGES, "bytes")
            .header(header::CONTENT_LENGTH, bytes.len().to_string())
            .body(Body::from(bytes))
            .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())
    }
}

fn parse_range(header: &str, len: u64) -> Option<(u64, u64)> {
    let spec = header.trim().strip_prefix("bytes=")?;
    let (start_s, end_s) = spec.split_once('-')?;
    if len == 0 {
        return None;
    }
    if start_s.is_empty() {
        let n: u64 = end_s.parse().ok()?;
        if n == 0 {
            return None;
        }
        let n = n.min(len);
        Some((len - n, len - 1))
    } else {
        let start: u64 = start_s.parse().ok()?;
        if start >= len {
            return None;
        }
        let end = if end_s.is_empty() {
            len - 1
        } else {
            end_s.parse::<u64>().ok()?.min(len - 1)
        };
        if end < start {
            return None;
        }
        Some((start, end))
    }
}

fn mime_of(path: &str) -> &'static str {
    let lower = path.to_ascii_lowercase();
    if lower.ends_with(".png") {
        "image/png"
    } else if lower.ends_with(".jpg") || lower.ends_with(".jpeg") {
        "image/jpeg"
    } else if lower.ends_with(".gif") {
        "image/gif"
    } else if lower.ends_with(".webp") {
        "image/webp"
    } else if lower.ends_with(".svg") {
        "image/svg+xml"
    } else if lower.ends_with(".mp4") {
        "video/mp4"
    } else if lower.ends_with(".webm") {
        "video/webm"
    } else if lower.ends_with(".pdf") {
        "application/pdf"
    } else {
        "application/octet-stream"
    }
}
