//! A minimal async HTTPS GET over `tokio-rustls` (ring) + Mozilla webpki roots
//! — the *same* crypto stack the Nostr relay layer uses, so the Android build
//! gains no second TLS/C dependency. Ported from ntrack's tile fetcher and
//! generalized to return bytes or text. Not a general HTTP client: one
//! connection, `Connection: close`, bounded read, de-chunking.

use std::sync::{Arc, OnceLock};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio_rustls::rustls::pki_types::ServerName;
use tokio_rustls::rustls::{ClientConfig, RootCertStore};
use tokio_rustls::TlsConnector;

use crate::error::{Error, Result};

/// OSM/Nominatim usage policy requires a descriptive, identifying `User-Agent`.
pub const USER_AGENT: &str = concat!(
    "nairobi2/",
    env!("CARGO_PKG_VERSION"),
    " (+https://github.com/felixboeck/nairobi2)"
);

/// Cap on a single response body (Nominatim/OSRM JSON, or a ~40 KB PNG tile).
const MAX_BODY: u64 = 4 * 1024 * 1024;

/// Install the rustls **ring** crypto provider as the process default, once.
/// rustls 0.23 requires a default provider for `ClientConfig::builder()`; the
/// relay layer may already have installed it, so we ignore "already set".
pub fn ensure_crypto_provider() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        let _ = tokio_rustls::rustls::crypto::ring::default_provider().install_default();
    });
}

/// Shared client TLS config (ring provider, Mozilla webpki roots), built once.
pub fn tls_config() -> Arc<ClientConfig> {
    static CONFIG: OnceLock<Arc<ClientConfig>> = OnceLock::new();
    CONFIG
        .get_or_init(|| {
            ensure_crypto_provider();
            let roots = RootCertStore {
                roots: webpki_roots::TLS_SERVER_ROOTS.to_vec(),
            };
            Arc::new(
                ClientConfig::builder()
                    .with_root_certificates(roots)
                    .with_no_client_auth(),
            )
        })
        .clone()
}

/// Perform one HTTPS GET to `host`/`path` and return the response body bytes.
/// `accept` sets the `Accept` header (`application/json`, `image/png`, …).
pub async fn https_get(host: &str, path: &str, accept: &str) -> Result<Vec<u8>> {
    let tls = tls_config();
    let stream = TcpStream::connect((host, 443))
        .await
        .map_err(|e| Error::Geo(format!("connect {host}: {e}")))?;
    let domain =
        ServerName::try_from(host.to_string()).map_err(|e| Error::Geo(format!("bad host {host}: {e}")))?;
    let connector = TlsConnector::from(tls);
    let mut tls_stream = connector
        .connect(domain, stream)
        .await
        .map_err(|e| Error::Geo(format!("tls {host}: {e}")))?;

    let request = format!(
        "GET {path} HTTP/1.1\r\n\
         Host: {host}\r\n\
         User-Agent: {USER_AGENT}\r\n\
         Accept: {accept}\r\n\
         Connection: close\r\n\r\n"
    );
    tls_stream
        .write_all(request.as_bytes())
        .await
        .map_err(|e| Error::Geo(format!("write {host}: {e}")))?;
    tls_stream
        .flush()
        .await
        .map_err(|e| Error::Geo(format!("flush {host}: {e}")))?;

    let mut raw = Vec::new();
    tls_stream
        .take(MAX_BODY)
        .read_to_end(&mut raw)
        .await
        .map_err(|e| Error::Geo(format!("read {host}: {e}")))?;

    parse_http_body(&raw).ok_or_else(|| Error::Geo(format!("bad HTTP response from {host}")))
}

/// Like [`https_get`] but returns the body as UTF-8 text (for JSON APIs).
pub async fn https_get_str(host: &str, path: &str) -> Result<String> {
    let bytes = https_get(host, path, "application/json").await?;
    String::from_utf8(bytes).map_err(|e| Error::Geo(format!("utf8 from {host}: {e}")))
}

/// Perform one HTTPS POST of a JSON `body` to `host`/`path` and return the
/// response body as UTF-8 text. Same single-connection, `Connection: close`,
/// bounded-read, de-chunking behaviour as [`https_get`] — used by the
/// proof-of-burn notary client (full webpki cert validation, unlike the
/// SPV-trust Electrum path).
pub async fn https_post_json(host: &str, path: &str, body: &str) -> Result<String> {
    let tls = tls_config();
    let stream = TcpStream::connect((host, 443))
        .await
        .map_err(|e| Error::Geo(format!("connect {host}: {e}")))?;
    let domain = ServerName::try_from(host.to_string())
        .map_err(|e| Error::Geo(format!("bad host {host}: {e}")))?;
    let mut tls_stream = TlsConnector::from(tls)
        .connect(domain, stream)
        .await
        .map_err(|e| Error::Geo(format!("tls {host}: {e}")))?;

    let request = format!(
        "POST {path} HTTP/1.1\r\n\
         Host: {host}\r\n\
         User-Agent: {USER_AGENT}\r\n\
         Accept: application/json\r\n\
         Content-Type: application/json\r\n\
         Content-Length: {len}\r\n\
         Connection: close\r\n\r\n{body}",
        len = body.len()
    );
    tls_stream
        .write_all(request.as_bytes())
        .await
        .map_err(|e| Error::Geo(format!("write {host}: {e}")))?;
    tls_stream
        .flush()
        .await
        .map_err(|e| Error::Geo(format!("flush {host}: {e}")))?;

    let mut raw = Vec::new();
    tls_stream
        .take(MAX_BODY)
        .read_to_end(&mut raw)
        .await
        .map_err(|e| Error::Geo(format!("read {host}: {e}")))?;

    let body = parse_http_body(&raw)
        .ok_or_else(|| Error::Geo(format!("bad HTTP response from {host}")))?;
    String::from_utf8(body).map_err(|e| Error::Geo(format!("utf8 from {host}: {e}")))
}

/// Split an HTTP/1.1 response into status/headers/body, returning the body only
/// on `200`, de-chunking when `Transfer-Encoding: chunked`.
fn parse_http_body(raw: &[u8]) -> Option<Vec<u8>> {
    let sep = find(raw, b"\r\n\r\n")?;
    let head = std::str::from_utf8(&raw[..sep]).ok()?;
    let body = &raw[sep + 4..];

    let mut lines = head.split("\r\n");
    let status = lines.next()?; // "HTTP/1.1 200 OK"
    if status.split_whitespace().nth(1) != Some("200") {
        return None;
    }
    let chunked = lines.any(|l| {
        let l = l.to_ascii_lowercase();
        l.starts_with("transfer-encoding:") && l.contains("chunked")
    });
    if chunked {
        dechunk(body)
    } else {
        Some(body.to_vec())
    }
}

/// Decode an HTTP/1.1 chunked body.
fn dechunk(mut data: &[u8]) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    loop {
        let nl = find(data, b"\r\n")?;
        let size_line = std::str::from_utf8(&data[..nl]).ok()?;
        let size_hex = size_line.split(';').next()?.trim(); // ignore chunk-ext
        let size = usize::from_str_radix(size_hex, 16).ok()?;
        data = &data[nl + 2..];
        if size == 0 {
            break;
        }
        if data.len() < size + 2 {
            return None;
        }
        out.extend_from_slice(&data[..size]);
        data = &data[size + 2..]; // skip chunk data + trailing CRLF
    }
    Some(out)
}

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn resp(head: &str, body: &str) -> Vec<u8> {
        format!("{head}\r\n\r\n{body}").into_bytes()
    }

    #[test]
    fn parses_plain_200_body() {
        let raw = resp("HTTP/1.1 200 OK\r\nContent-Type: application/json", "{\"ok\":1}");
        assert_eq!(parse_http_body(&raw).unwrap(), b"{\"ok\":1}");
    }

    #[test]
    fn rejects_non_200() {
        let raw = resp("HTTP/1.1 404 Not Found", "nope");
        assert!(parse_http_body(&raw).is_none());
    }

    #[test]
    fn dechunks_chunked_body() {
        // "Wiki" + "pedia" in two chunks.
        let raw = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n4\r\nWiki\r\n5\r\npedia\r\n0\r\n\r\n";
        assert_eq!(parse_http_body(raw).unwrap(), b"Wikipedia");
    }

    #[test]
    fn user_agent_is_descriptive() {
        assert!(USER_AGENT.starts_with("nairobi2/"));
        assert!(USER_AGENT.contains("https://"));
    }
}
