//! Part A — the notary HTTP client. Requests notarization of a Nostr event,
//! returns the BOLT-11 invoice to pay, and fetches the resulting proof once the
//! invoice settles. See `docs/proof-of-burn-api.md` §5.
//!
//! Transport is the app's webpki-validated HTTPS POST (`crate::geo::http`) — the
//! notary is a normal CA-certificate web service, unlike the self-signing
//! Electrum servers. Not host-testable offline; the request/response *shaping*
//! is pure and unit-tested.

use serde::Deserialize;

use super::proof::{proof_from_json, BurnProof};
use super::{NOTARY_API_BASE, NOTARY_HOST};
use crate::error::{Error, Result};
use crate::geo::http::https_post_json;

/// A notary endpoint (host + API base path). Defaults to `notary.electrum.org`.
#[derive(Clone, Debug)]
pub struct NotaryClient {
    pub host: String,
    pub base: String,
}

impl Default for NotaryClient {
    fn default() -> Self {
        Self {
            host: NOTARY_HOST.to_string(),
            base: NOTARY_API_BASE.to_string(),
        }
    }
}

/// The `add_request` response: an invoice to pay and its payment hash (the
/// handle for `get_proof`).
#[derive(Clone, Debug, Deserialize)]
pub struct AddRequestResponse {
    pub invoice: String,
    pub rhash: String,
}

impl NotaryClient {
    /// The public notary (`notary.electrum.org`).
    pub fn public() -> Self {
        Self::default()
    }

    /// Request notarization of `event_id_hex`, burning `value_sats`. `upvoter`
    /// optionally claims authorship `(pubkey_hex, signature_hex)`. Returns the
    /// invoice to pay + the `rhash` handle.
    pub async fn add_request(
        &self,
        event_id_hex: &str,
        value_sats: u64,
        nonce_hex: &str,
        upvoter: Option<(&str, &str)>,
    ) -> Result<AddRequestResponse> {
        let body = build_add_request_body(event_id_hex, value_sats, nonce_hex, upvoter);
        let path = format!("{}/add_request", self.base);
        let text = https_post_json(&self.host, &path, &body).await?;
        parse_add_request(&text)
    }

    /// Fetch the proof for `rhash`. Returns `Ok(None)` while the invoice is
    /// unpaid / the batch isn't ready (the notary replies with a transient
    /// `error`); `Ok(Some(proof))` once available.
    pub async fn get_proof(&self, rhash: &str) -> Result<Option<BurnProof>> {
        let body = serde_json::json!({ "rhash": rhash }).to_string();
        let path = format!("{}/get_proof", self.base);
        let text = https_post_json(&self.host, &path, &body).await?;
        parse_get_proof(&text)
    }
}

/// Build the `add_request` JSON body (`docs/proof-of-burn-api.md` §5.2).
fn build_add_request_body(
    event_id_hex: &str,
    value_sats: u64,
    nonce_hex: &str,
    upvoter: Option<(&str, &str)>,
) -> String {
    let mut obj = serde_json::json!({
        "event_id": event_id_hex,
        "value_sats": value_sats,
        "nonce": nonce_hex,
    });
    if let Some((pk, sig)) = upvoter {
        obj["upvoter_pubkey"] = serde_json::Value::String(pk.to_string());
        obj["upvoter_signature"] = serde_json::Value::String(sig.to_string());
    }
    obj.to_string()
}

fn parse_add_request(text: &str) -> Result<AddRequestResponse> {
    let v: serde_json::Value =
        serde_json::from_str(text).map_err(|e| Error::Burn(format!("add_request json: {e}")))?;
    if let Some(err) = v.get("error").and_then(|e| e.as_str()) {
        return Err(Error::Burn(format!("notary add_request: {err}")));
    }
    serde_json::from_value(v).map_err(|e| Error::Burn(format!("add_request shape: {e}")))
}

/// `None` for a transient "waiting for payment" error; `Some(proof)` otherwise.
fn parse_get_proof(text: &str) -> Result<Option<BurnProof>> {
    let v: serde_json::Value =
        serde_json::from_str(text).map_err(|e| Error::Burn(format!("get_proof json: {e}")))?;
    if let Some(err) = v.get("error").and_then(|e| e.as_str()) {
        // Payment not yet seen / batch not ready → not an error, just not ready.
        if err.to_ascii_lowercase().contains("wait") {
            return Ok(None);
        }
        return Err(Error::Burn(format!("notary get_proof: {err}")));
    }
    Ok(Some(proof_from_json(&v)?))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::burn::proof::testtree::mint_proof;
    use crate::burn::to_hex;

    #[test]
    fn add_request_body_includes_upvoter_only_when_signed() {
        let anon = build_add_request_body("aa", 42, "bb", None);
        let v: serde_json::Value = serde_json::from_str(&anon).unwrap();
        assert_eq!(v["value_sats"], 42);
        assert!(v.get("upvoter_pubkey").is_none());

        let signed = build_add_request_body("aa", 42, "bb", Some(("cc", "dd")));
        let v: serde_json::Value = serde_json::from_str(&signed).unwrap();
        assert_eq!(v["upvoter_pubkey"], "cc");
        assert_eq!(v["upvoter_signature"], "dd");
    }

    #[test]
    fn parse_add_request_extracts_invoice_and_rhash() {
        let ok = r#"{"invoice":"lnbc420n1...","rhash":"a5d29d8e"}"#;
        let r = parse_add_request(ok).unwrap();
        assert_eq!(r.invoice, "lnbc420n1...");
        assert_eq!(r.rhash, "a5d29d8e");

        assert!(parse_add_request(r#"{"error":"bad signature"}"#).is_err());
    }

    #[test]
    fn get_proof_distinguishes_waiting_from_ready() {
        // Waiting → Ok(None).
        let waiting = r#"{"error":"Waiting for payment"}"#;
        assert!(parse_get_proof(waiting).unwrap().is_none());

        // A real proof object → Ok(Some).
        let (proof, _root) = mint_proof([1u8; 32], 7, [2u8; 32], None, &[([3u8; 32], 3000)], "ab".repeat(32).as_str(), 0);
        let json = serde_json::json!({
            "version": 0,
            "event_id": to_hex(&proof.event_id),
            "txid": proof.txid,
            "block_height": 0,
            "nonce": to_hex(&proof.nonce),
            "leaf_value": proof.leaf_value_msat,
            "merkle_index": proof.merkle_index,
            "merkle_hashes": proof.merkle_hashes.iter().map(|(h,v)| format!("{}:{}", to_hex(h), v)).collect::<Vec<_>>(),
        })
        .to_string();
        let got = parse_get_proof(&json).unwrap().unwrap();
        assert_eq!(got.event_id, proof.event_id);
    }
}
