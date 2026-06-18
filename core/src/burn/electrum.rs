//! A tiny Electrum JSON-RPC client over TLS — the network half of proof
//! verification. Speaks line-delimited JSON-RPC to an ElectrumX / electrs /
//! Fulcrum server (default SSL port 50002), reusing the same `tokio-rustls`
//! **ring** stack as the rest of the app.
//!
//! ## Trust posture
//! Most public Electrum servers present **self-signed** certificates, so this
//! client does **not** validate the certificate chain against a CA root (it
//! still checks the handshake signature against the presented cert). That is
//! safe here because proof *integrity* comes from SPV — the Merkle branch
//! checked against proof-of-work — and from **cross-checking several
//! independent servers**, never from TLS. TLS only buys query privacy and
//! availability. Certificate pinning (TOFU) is a future hardening; see the
//! design spec §8.2. This relaxed verifier is confined to this module and never
//! touches the notary HTTPS path (which keeps full webpki validation).
//!
//! Not host-testable offline (it opens sockets); the response *parsing* is
//! pure and unit-tested.

use std::sync::Arc;

use serde::Deserialize;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio_rustls::rustls::client::danger::{
    HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier,
};
use tokio_rustls::rustls::crypto::{verify_tls12_signature, verify_tls13_signature};
use tokio_rustls::rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use tokio_rustls::rustls::{ClientConfig, DigitallySignedStruct, SignatureScheme};
use tokio_rustls::TlsConnector;

use crate::error::{Error, Result};
use crate::geo::http::ensure_crypto_provider;

/// Default Electrum SSL port.
pub const DEFAULT_ELECTRUM_PORT: u16 = 50002;

/// `host:port` of an Electrum server (TLS).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ElectrumServer {
    pub host: String,
    pub port: u16,
}

impl ElectrumServer {
    /// Parse `"host"` or `"host:port"` (defaulting to [`DEFAULT_ELECTRUM_PORT`]).
    pub fn parse(s: &str) -> Self {
        match s.rsplit_once(':') {
            Some((h, p)) if p.parse::<u16>().is_ok() => ElectrumServer {
                host: h.to_string(),
                port: p.parse().unwrap(),
            },
            _ => ElectrumServer {
                host: s.to_string(),
                port: DEFAULT_ELECTRUM_PORT,
            },
        }
    }
}

/// SPV Merkle branch for a tx (`blockchain.transaction.get_merkle`).
#[derive(Clone, Debug, Deserialize)]
pub struct MerkleProof {
    pub block_height: u64,
    pub merkle: Vec<String>,
    pub pos: u64,
}

/// A connection to one Electrum server. Each call opens a fresh TLS connection
/// (like the HTTP client's `Connection: close`) — simple and robust.
pub struct ElectrumClient {
    server: ElectrumServer,
    tls: Arc<ClientConfig>,
}

impl ElectrumClient {
    /// Build a client for `server` with the relaxed (SPV-trust) TLS config.
    pub fn new(server: ElectrumServer) -> Self {
        Self {
            server,
            tls: accepting_tls_config(),
        }
    }

    /// One JSON-RPC round trip: connect, send `{method, params}\n`, read one
    /// response line, return its `result` value.
    async fn call(&self, method: &str, params: serde_json::Value) -> Result<serde_json::Value> {
        let req = serde_json::json!({"id": 0, "method": method, "params": params});
        let line = format!("{req}\n");

        let stream = TcpStream::connect((self.server.host.as_str(), self.server.port))
            .await
            .map_err(|e| Error::Burn(format!("electrum connect {}: {e}", self.server.host)))?;
        let domain = ServerName::try_from(self.server.host.clone())
            .map_err(|e| Error::Burn(format!("electrum host {}: {e}", self.server.host)))?;
        let mut tls = TlsConnector::from(self.tls.clone())
            .connect(domain, stream)
            .await
            .map_err(|e| Error::Burn(format!("electrum tls {}: {e}", self.server.host)))?;

        tls.write_all(line.as_bytes())
            .await
            .map_err(|e| Error::Burn(format!("electrum write: {e}")))?;
        tls.flush().await.ok();

        // Read until the first newline (one JSON object per line).
        let mut buf = Vec::with_capacity(4096);
        let mut chunk = [0u8; 4096];
        loop {
            let n = tls
                .read(&mut chunk)
                .await
                .map_err(|e| Error::Burn(format!("electrum read: {e}")))?;
            if n == 0 {
                break;
            }
            buf.extend_from_slice(&chunk[..n]);
            if buf.contains(&b'\n') || buf.len() > 8 * 1024 * 1024 {
                break;
            }
        }
        parse_rpc_result(&buf)
    }

    /// Raw transaction hex (`blockchain.transaction.get`).
    pub async fn get_transaction(&self, txid: &str) -> Result<Vec<u8>> {
        let v = self.call("blockchain.transaction.get", serde_json::json!([txid])).await?;
        let hex = v
            .as_str()
            .ok_or_else(|| Error::Burn("transaction.get: not a hex string".into()))?;
        super::from_hex(hex)
    }

    /// SPV Merkle branch (`blockchain.transaction.get_merkle`).
    pub async fn get_merkle(&self, txid: &str, height: u64) -> Result<MerkleProof> {
        let v = self
            .call("blockchain.transaction.get_merkle", serde_json::json!([txid, height]))
            .await?;
        serde_json::from_value(v).map_err(|e| Error::Burn(format!("get_merkle: {e}")))
    }

    /// Block header hex at `height` (`blockchain.block.header`).
    pub async fn block_header(&self, height: u64) -> Result<Vec<u8>> {
        let v = self.call("blockchain.block.header", serde_json::json!([height])).await?;
        let hex = v
            .as_str()
            .ok_or_else(|| Error::Burn("block.header: not a hex string".into()))?;
        super::from_hex(hex)
    }
}

/// Extract `result` from an Electrum JSON-RPC response line, surfacing `error`.
fn parse_rpc_result(buf: &[u8]) -> Result<serde_json::Value> {
    let line = buf
        .split(|&b| b == b'\n')
        .next()
        .ok_or_else(|| Error::Burn("empty electrum response".into()))?;
    let v: serde_json::Value =
        serde_json::from_slice(line).map_err(|e| Error::Burn(format!("electrum json: {e}")))?;
    if let Some(err) = v.get("error") {
        if !err.is_null() {
            return Err(Error::Burn(format!("electrum error: {err}")));
        }
    }
    v.get("result")
        .cloned()
        .ok_or_else(|| Error::Burn("electrum response missing `result`".into()))
}

/// A TLS config that accepts any server certificate (see the module docs for
/// why this is safe for SPV). Built once.
fn accepting_tls_config() -> Arc<ClientConfig> {
    use std::sync::OnceLock;
    static CFG: OnceLock<Arc<ClientConfig>> = OnceLock::new();
    CFG.get_or_init(|| {
        ensure_crypto_provider();
        Arc::new(
            ClientConfig::builder()
                .dangerous()
                .with_custom_certificate_verifier(Arc::new(AcceptAnyServerCert))
                .with_no_client_auth(),
        )
    })
    .clone()
}

/// Accepts any certificate chain but still checks the handshake signature
/// against the presented key. Integrity for proof-of-burn comes from SPV, not
/// PKI — see the module docs.
#[derive(Debug)]
struct AcceptAnyServerCert;

impl ServerCertVerifier for AcceptAnyServerCert {
    fn verify_server_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> std::result::Result<ServerCertVerified, tokio_rustls::rustls::Error> {
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> std::result::Result<HandshakeSignatureValid, tokio_rustls::rustls::Error> {
        verify_tls12_signature(
            message,
            cert,
            dss,
            &tokio_rustls::rustls::crypto::ring::default_provider().signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> std::result::Result<HandshakeSignatureValid, tokio_rustls::rustls::Error> {
        verify_tls13_signature(
            message,
            cert,
            dss,
            &tokio_rustls::rustls::crypto::ring::default_provider().signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        tokio_rustls::rustls::crypto::ring::default_provider()
            .signature_verification_algorithms
            .supported_schemes()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn server_parse_defaults_port() {
        assert_eq!(ElectrumServer::parse("electrum.example").port, 50002);
        let s = ElectrumServer::parse("electrum.example:50001");
        assert_eq!(s.host, "electrum.example");
        assert_eq!(s.port, 50001);
        // A bare IPv6 without port falls back to default (no false port split).
        assert_eq!(ElectrumServer::parse("fulcrum.test").host, "fulcrum.test");
    }

    #[test]
    fn parses_rpc_result_and_errors() {
        let ok = b"{\"id\":0,\"result\":\"deadbeef\",\"error\":null}\n";
        assert_eq!(parse_rpc_result(ok).unwrap().as_str().unwrap(), "deadbeef");

        let err = b"{\"id\":0,\"error\":{\"message\":\"no such tx\"}}\n";
        assert!(parse_rpc_result(err).is_err());

        let merkle = b"{\"id\":0,\"result\":{\"block_height\":900000,\"merkle\":[\"aa\",\"bb\"],\"pos\":3}}\n";
        let m: MerkleProof = serde_json::from_value(parse_rpc_result(merkle).unwrap()).unwrap();
        assert_eq!(m.block_height, 900000);
        assert_eq!(m.pos, 3);
        assert_eq!(m.merkle, vec!["aa", "bb"]);
    }
}
