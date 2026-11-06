//! Update distribution (S7 step 5, ADR 0016). The server holds no signing key: it
//! serves the signed envelope and the files the release tool produced in
//! `--update-dir` (`manifest.json`, `files/<version>/<path>`). Clients verify both.
//! No authentication (signed-out clients and the installer need it); concurrent file
//! downloads are limited.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use axum::body::Body;
use axum::extract::{Path as UrlPath, State};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use tokio::sync::Semaphore;
use turbo_pancake_update::Version;
use turbo_pancake_update::manifest::{MAX_ENVELOPE_BYTES, valid_path};

use crate::AppState;

/// Simultaneous file downloads (a release is ~25 MB; protects the uplink and fds).
pub const MAX_DOWNLOADS: usize = 8;
const CHUNK: usize = 256 * 1024;

pub struct Updates {
    pub dir: PathBuf,
    pub downloads: Arc<Semaphore>,
}

impl Updates {
    pub fn new(dir: PathBuf) -> Self {
        Self {
            dir,
            downloads: Arc::new(Semaphore::new(MAX_DOWNLOADS)),
        }
    }
}

pub async fn manifest(State(state): State<AppState>) -> Response {
    let Some(u) = &state.updates else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let path = u.dir.join("manifest.json");
    let read = tokio::task::spawn_blocking(move || -> std::io::Result<Option<Vec<u8>>> {
        let meta = match std::fs::metadata(&path) {
            Ok(m) => m,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e),
        };
        if meta.len() > MAX_ENVELOPE_BYTES as u64 {
            return Err(std::io::Error::other("manifest too large"));
        }
        std::fs::read(&path).map(Some)
    })
    .await;
    match read {
        Ok(Ok(Some(bytes))) => (
            [
                (header::CONTENT_TYPE, "application/json"),
                (header::CACHE_CONTROL, "no-cache"),
            ],
            bytes,
        )
            .into_response(),
        Ok(Ok(None)) => StatusCode::NOT_FOUND.into_response(),
        Ok(Err(e)) => {
            tracing::error!(error = %e, "update manifest unreadable");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
        Err(_) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}

/// The file below `dir` for a request, or None for anything outside the rules.
pub fn file_path(dir: &Path, version: &str, path: &str) -> Option<PathBuf> {
    (Version::parse(version).is_some() && valid_path(path))
        .then(|| dir.join("files").join(version).join(path))
}

pub async fn file(
    State(state): State<AppState>,
    UrlPath((version, path)): UrlPath<(String, String)>,
) -> Response {
    let Some(u) = &state.updates else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let Some(full) = file_path(&u.dir, &version, &path) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let Ok(permit) = u.downloads.clone().try_acquire_owned() else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            [(header::RETRY_AFTER, "30")],
        )
            .into_response();
    };
    let opened = tokio::task::spawn_blocking(move || {
        let f = std::fs::File::open(&full)?;
        let len = f.metadata()?.len();
        if !f.metadata()?.is_file() {
            return Err(std::io::Error::other("not a file"));
        }
        Ok((f, len))
    })
    .await;
    let (file, len) = match opened {
        Ok(Ok(v)) => v,
        _ => return StatusCode::NOT_FOUND.into_response(),
    };
    // Chunks read on the blocking pool; the permit lives as long as the stream.
    let stream = futures_util::stream::unfold((Some(file), permit), |(file, permit)| async move {
        let mut f = file?;
        let res = tokio::task::spawn_blocking(move || {
            use std::io::Read;
            let mut buf = vec![0u8; CHUNK];
            let n = f.read(&mut buf)?;
            buf.truncate(n);
            Ok::<_, std::io::Error>((f, buf))
        })
        .await
        .unwrap_or_else(|e| Err(std::io::Error::other(e)));
        match res {
            Ok((_, buf)) if buf.is_empty() => None,
            Ok((f, buf)) => Some((Ok(buf), (Some(f), permit))),
            Err(e) => Some((Err(e), (None, permit))),
        }
    });
    (
        [
            (header::CONTENT_TYPE, "application/octet-stream".to_string()),
            (header::CONTENT_LENGTH, len.to_string()),
        ],
        Body::from_stream(stream),
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_rule_conforming_paths_map_to_files() {
        let d = Path::new("upd");
        assert_eq!(
            file_path(d, "0.1.1", "turbo-pancake-desktop.exe"),
            Some(
                d.join("files")
                    .join("0.1.1")
                    .join("turbo-pancake-desktop.exe")
            )
        );
        assert!(file_path(d, "0.1.1", "res/a.bin").is_some());
        for (v, p) in [
            ("0.1.1", "../manifest.json"),
            ("..", "x"),
            ("0.1.1", "a\\..\\b"),
            ("0.1", "x"),
            ("0.1.1", "C:/x"),
            ("0.1.1", ""),
        ] {
            assert_eq!(file_path(d, v, p), None, "{v} {p}");
        }
    }
}
