//! A loopback file server for the webview.
//!
//! Tauri's `asset:` protocol reads a whole file into memory when the request carries no
//! `Range` header, and has open bugs around seeking large media. Group synchronisation
//! seeks. So instead the media directory is served over `127.0.0.1` on an ephemeral port,
//! streaming, with the same Range semantics as the management server's own
//! `media.controller.ts` — which is the behaviour the download side already understands.
//!
//! A random token in the path keeps other processes on the machine out. It is not a
//! security boundary, and is not pretending to be one: it stops an accident, not an
//! attacker with the same user account.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::thread;

use anyhow::{anyhow, Result};
use tiny_http::{Header, Request, Response, Server, StatusCode};

use crate::{ldebug, linfo, lwarn};

pub struct MediaServer {
    /// Everything before the filename, e.g. `http://127.0.0.1:52341/9f3a…`.
    pub base_url: String,
}

impl MediaServer {
    /// The URL the webview should load, stamped with the file's modification time.
    ///
    /// The stamp is what makes it safe to serve the media as immutable: a re-download of
    /// the *same* videoId produces a different stamp, so the webview fetches the new
    /// bytes instead of replaying the ones it cached.
    pub fn url_for(&self, file: &Path) -> Option<String> {
        let name = file.file_name()?.to_str()?;
        let stamp = std::fs::metadata(file)
            .and_then(|meta| meta.modified())
            .ok()
            .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|since| since.as_millis())
            .unwrap_or(0);
        Some(format!("{}/{}?v={}", self.base_url, name, stamp))
    }
}

pub fn spawn(media_dir: PathBuf) -> Result<MediaServer> {
    let token = uuid::Uuid::new_v4().simple().to_string();
    let addr = SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0));
    let server = Server::http(addr).map_err(|err| anyhow!("media server: {err}"))?;

    let port = server
        .server_addr()
        .to_ip()
        .ok_or_else(|| anyhow!("media server bound to a non-IP address"))?
        .port();
    let base_url = format!("http://127.0.0.1:{port}/{token}");
    linfo!("Media server on 127.0.0.1:{port}, serving {}", media_dir.display());

    let server = Arc::new(server);
    let prefix = format!("/{token}/");
    thread::Builder::new()
        .name("media-server".into())
        .spawn(move || {
            for request in server.incoming_requests() {
                if let Err(err) = handle(&media_dir, &prefix, request) {
                    // One bad request must never take the server down; the screen would
                    // go black for a reason nobody could see.
                    lwarn!("Media request failed: {err}");
                }
            }
        })?;

    Ok(MediaServer { base_url })
}

fn handle(media_dir: &Path, prefix: &str, request: Request) -> Result<()> {
    let url = request.url().to_string();
    let path = url.split('?').next().unwrap_or("");

    let Some(name) = path.strip_prefix(prefix) else {
        return respond_plain(request, 404, "not found\n");
    };

    // `file_name` on the untrusted segment is the traversal guard, the same one
    // `media.controller.ts` uses with `path.basename`.
    let Some(name) = Path::new(name).file_name().and_then(|n| n.to_str()) else {
        return respond_plain(request, 404, "not found\n");
    };

    let file_path = media_dir.join(name);
    let Ok(meta) = std::fs::metadata(&file_path) else {
        return respond_plain(request, 404, "not found\n");
    };
    if !meta.is_file() {
        return respond_plain(request, 404, "not found\n");
    }
    let size = meta.len();

    let range = request
        .headers()
        .iter()
        .find(|h| h.field.equiv("Range"))
        .map(|h| h.value.as_str().to_string());

    // A malformed Range is ignored rather than rejected, per RFC 7233 and the server's
    // own behaviour, so a client that sends nonsense still gets its video.
    let parsed = range.as_deref().and_then(parse_range);

    let mut headers = vec![
        header("Content-Type", "video/mp4"),
        header("Accept-Ranges", "bytes"),
        // Cacheable on purpose. With `no-store` the media element re-fetched the whole
        // file on every loop — 4 MB every ten seconds for a short campaign, and far worse
        // for a long one. Freshness comes from the `?v=` stamp in the URL instead, which
        // changes whenever the file behind a given name does.
        header("Cache-Control", "private, max-age=31536000, immutable"),
    ];

    let (status, start, length) = match parsed {
        Some((start, _)) if start >= size => {
            headers.push(header("Content-Range", &format!("bytes */{size}")));
            let response = Response::new(StatusCode(416), headers, std::io::empty(), Some(0), None);
            return request.respond(response).map_err(Into::into);
        }
        Some((start, end)) => {
            let end = end.unwrap_or(size - 1).min(size - 1);
            headers.push(header(
                "Content-Range",
                &format!("bytes {start}-{end}/{size}"),
            ));
            (206u16, start, end - start + 1)
        }
        None => (200u16, 0, size),
    };

    ldebug!("Media {name}: {status}, bytes {start}..{}", start + length);

    let mut file = File::open(&file_path)?;
    file.seek(SeekFrom::Start(start))?;
    let body = file.take(length);
    let response = Response::new(
        StatusCode(status),
        headers,
        body,
        Some(length as usize),
        None,
    )
    // tiny_http switches to chunked encoding above 32 KB by default, which drops
    // Content-Length — and a media element wants Content-Length and Content-Range to
    // seek. The body is streamed from the file either way, so nothing is buffered by
    // raising this.
    .with_chunked_threshold(usize::MAX);
    request.respond(response).map_err(Into::into)
}

fn respond_plain(request: Request, status: u16, body: &str) -> Result<()> {
    let response = Response::from_string(body)
        .with_status_code(StatusCode(status))
        .with_header(header("Content-Type", "text/plain"));
    request.respond(response).map_err(Into::into)
}

fn header(field: &str, value: &str) -> Header {
    // Both sides are string literals or numbers we just formatted, so this cannot fail.
    Header::from_bytes(field.as_bytes(), value.as_bytes())
        .expect("static header is well formed")
}

/// `bytes=N-` or `bytes=N-M`. Anything else — suffix ranges, multiple ranges, a different
/// unit — returns None, which the caller turns into a plain 200.
fn parse_range(value: &str) -> Option<(u64, Option<u64>)> {
    let spec = value.trim().strip_prefix("bytes=")?;
    let (start, end) = spec.split_once('-')?;
    if start.is_empty() {
        return None;
    }
    let start: u64 = start.parse().ok()?;
    let end = if end.is_empty() {
        None
    } else {
        Some(end.parse().ok()?)
    };
    if end.is_some_and(|e| e < start) {
        return None;
    }
    Some((start, end))
}

#[cfg(test)]
mod tests {
    use super::parse_range;

    #[test]
    fn open_ended_range() {
        assert_eq!(parse_range("bytes=100000-"), Some((100_000, None)));
    }

    #[test]
    fn closed_range() {
        assert_eq!(parse_range("bytes=100-199"), Some((100, Some(199))));
    }

    #[test]
    fn malformed_ranges_are_ignored_not_rejected() {
        assert_eq!(parse_range("llamas=1-2"), None);
        assert_eq!(parse_range("bytes=-500"), None, "suffix ranges are not supported");
        assert_eq!(parse_range("bytes=abc-"), None);
        assert_eq!(parse_range("bytes=10-5"), None, "end before start");
        assert_eq!(parse_range("bytes=0-1,4-5"), None, "multipart");
    }
}
