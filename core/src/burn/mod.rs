//! Proof-of-burn — make publishing the public Nostr events cost real money, so
//! Sybil spam becomes linearly expensive and identities accrue durable, portable
//! reputation.
//!
//! A **proof-of-burn** is a publicly verifiable statement that a specific Nostr
//! `event_id` was committed to the Bitcoin blockchain inside a transaction that
//! irreversibly sacrifices satoshis to the miners. Proofs are produced by a
//! **notary** ([`notary.electrum.org`], running [`spesmilo/notary`]), paid for
//! over Lightning by the app's [`crate::wallet::Wallet`], and **verified
//! client-side** against Electrum indexing servers — the notary is trusted only
//! for liveness and for actually burning the funds, never for proof *validity*
//! (that is checked on-chain here).
//!
//! See `docs/proof-of-burn-api.md` for the wire protocol and
//! `docs/superpowers/specs/2026-06-18-proof-of-burn-antisybil-design.md` for how
//! nairobi2 uses it (identity bond → reputation gate → per-ride accrual).
//!
//! ## Layout
//! - [`proof`] — the canonical primitives (leaf/node hashing, Merkle-sum root
//!   reconstruction), the [`proof::BurnProof`] object, and kind-30021 packing.
//!   Pure, exhaustively host-tested.
//! - [`tx`] — a minimal Bitcoin transaction/script parser: txid, the `OP_RETURN`
//!   commitment, and the P2WSH burn output. Pure.
//! - [`verify`] — Part B: bind a proof to its on-chain burn. Pure given the tx
//!   bytes; the network half (fetching the tx / SPV) lives in [`electrum`].
//! - [`reputation`] — per-pubkey accrual of verified burns (dedup by leaf hash,
//!   confirmed-only, counterparty diversity). Pure.
//! - [`electrum`] / [`notary`] — the I/O clients (Electrum JSON-RPC over TLS;
//!   the notary HTTP API). Not host-testable offline; their parsing is.
//! - [`service`] — the [`service::BurnService`] seam (mirrors [`crate::pool`] /
//!   [`crate::wallet`]): a `MockBurnService` for tests and the real
//!   notary+wallet+Electrum implementation.
//!
//! [`notary.electrum.org`]: https://notary.electrum.org
//! [`spesmilo/notary`]: https://github.com/spesmilo/notary

pub mod electrum;
pub mod notary;
pub mod proof;
pub mod reputation;
pub mod service;
pub mod tx;
pub mod verify;
pub mod watch;

/// `OP_RETURN` magic prefix identifying a notarization commitment.
pub const MAGIC_BYTES: [u8; 2] = [0x00, 0x21];
/// Current proof-format version emitted by the notary.
pub const PROOF_VERSION: u32 = 0;
/// Literal leaf-hash domain prefix (`"Leaf:"`).
pub const LEAF_PREFIX: &[u8] = b"Leaf:";
/// Literal node-hash domain prefix (`"Node:"`).
pub const NODE_PREFIX: &[u8] = b"Node:";
/// The notary's default `OP_CHECKSEQUENCEVERIFY` delay.
pub const DEFAULT_CSV_DELAY: u16 = 144;

/// The public notary's host and API base path (the deployment proxies the
/// plugin's `/r` routes under `/n`). Used by [`notary`].
pub const NOTARY_HOST: &str = "notary.electrum.org";
/// The path prefix the public notary serves its JSON API under.
pub const NOTARY_API_BASE: &str = "/n/api";

/// The notary's fee on top of a `value_sats` burn (the invoice charges
/// `value_sats + notary_fee(value_sats)`). A step schedule that makes tiny
/// burns inefficient and larger burns cheap — see `docs/proof-of-burn-api.md`
/// §5.2.
pub const fn notary_fee(value_sats: u64) -> u64 {
    match value_sats {
        0..=8 => value_sats,
        9..=32 => value_sats / 2,
        33..=256 => value_sats / 4,
        _ => value_sats / 8,
    }
}

// ---- small hex helpers (no dependency; the project hand-rolls these) -------

/// Lower-case hex of `bytes`.
pub(crate) fn to_hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut s = String::with_capacity(bytes.len() * 2);
    for &b in bytes {
        s.push(HEX[(b >> 4) as usize] as char);
        s.push(HEX[(b & 0x0f) as usize] as char);
    }
    s
}

/// Decode hex into a `Vec<u8>`, erroring on odd length or non-hex digits.
pub(crate) fn from_hex(s: &str) -> crate::Result<Vec<u8>> {
    let s = s.trim();
    if !s.len().is_multiple_of(2) {
        return Err(crate::Error::Burn(format!("odd-length hex ({})", s.len())));
    }
    fn nyb(c: u8) -> Option<u8> {
        match c {
            b'0'..=b'9' => Some(c - b'0'),
            b'a'..=b'f' => Some(c - b'a' + 10),
            b'A'..=b'F' => Some(c - b'A' + 10),
            _ => None,
        }
    }
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(s.len() / 2);
    let mut i = 0;
    while i < b.len() {
        let hi = nyb(b[i]).ok_or_else(|| crate::Error::Burn("bad hex digit".into()))?;
        let lo = nyb(b[i + 1]).ok_or_else(|| crate::Error::Burn("bad hex digit".into()))?;
        out.push((hi << 4) | lo);
        i += 2;
    }
    Ok(out)
}

/// Decode exactly `N` bytes of hex into a fixed array.
pub(crate) fn from_hex_array<const N: usize>(s: &str) -> crate::Result<[u8; N]> {
    let v = from_hex(s)?;
    if v.len() != N {
        return Err(crate::Error::Burn(format!(
            "expected {N} bytes, got {}",
            v.len()
        )));
    }
    let mut a = [0u8; N];
    a.copy_from_slice(&v);
    Ok(a)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fee_schedule_matches_doc() {
        assert_eq!(notary_fee(8), 8); // x ≤ 8 → x
        assert_eq!(notary_fee(32), 16); // ≤ 32 → x/2
        assert_eq!(notary_fee(256), 64); // ≤ 256 → x/4
        assert_eq!(notary_fee(1000), 125); // > 256 → x/8
    }

    #[test]
    fn hex_round_trips() {
        let bytes = [0x00u8, 0x21, 0xde, 0xad, 0xbe, 0xef, 0xff];
        let hex = to_hex(&bytes);
        assert_eq!(hex, "0021deadbeefff");
        assert_eq!(from_hex(&hex).unwrap(), bytes);
        assert_eq!(from_hex_array::<7>(&hex).unwrap(), bytes);
        assert!(from_hex("0").is_err()); // odd length
        assert!(from_hex("zz").is_err()); // non-hex
        assert!(from_hex_array::<3>("deadbeef").is_err()); // wrong length
    }
}
