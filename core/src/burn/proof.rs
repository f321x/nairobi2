//! Canonical proof-of-burn primitives and the [`BurnProof`] object.
//!
//! Everything here must match the notary's bytes exactly — a single different
//! byte breaks every hash. Values circulate inside the tree in **millisatoshis**
//! (`leaf_value`, node sums); only the *root* (the on-chain burn output) is in
//! whole satoshis. Encodings follow `docs/proof-of-burn-api.md` §2, which is
//! written against the running plugin code:
//!
//! ```text
//! leaf_hash = SHA256( "Leaf:" || event_id(32) || value_msat(8 BE)
//!                            || nonce(32) || (upvoter_pubkey or 0x00*32)(32) )
//! ```
//!
//! ⚠️ The field order is `nonce` **then** `pubkey`. The whitepaper prints
//! `pubkey || nonce`; that is the academic ordering and is *wrong* for the
//! deployed notary. Follow the code.

use nostr_sdk::hashes::{sha256, Hash, HashEngine};

use super::{from_hex_array, to_hex, LEAF_PREFIX, NODE_PREFIX, PROOF_VERSION};
use crate::error::{Error, Result};

/// A 32-byte value (event id, nonce, tree node hash, x-only pubkey).
pub type B32 = [u8; 32];

/// 8-byte big-endian encoding of a tree value (always in msat).
#[inline]
pub fn int_to_bytes(x: u64) -> [u8; 8] {
    x.to_be_bytes()
}

#[inline]
fn sha256(parts: &[&[u8]]) -> B32 {
    let mut eng = sha256::Hash::engine();
    for p in parts {
        eng.input(p);
    }
    sha256::Hash::from_engine(eng).to_byte_array()
}

/// `leaf_hash(event_id, leaf_value_msat, nonce, upvoter_pubkey)`. Anonymous
/// upvotes pass `None` (32 zero bytes are hashed instead).
pub fn leaf_hash(event_id: &B32, leaf_value_msat: u64, nonce: &B32, upvoter_pubkey: Option<&B32>) -> B32 {
    let zeros = [0u8; 32];
    let pk = upvoter_pubkey.unwrap_or(&zeros);
    sha256(&[
        LEAF_PREFIX,
        event_id,
        &int_to_bytes(leaf_value_msat),
        nonce,
        pk,
    ])
}

/// `node_hash` of an inner Merkle-sum node; the node value is `left_v + right_v`
/// (msat), computed by the caller.
pub fn node_hash(left_h: &B32, left_v: u64, right_h: &B32, right_v: u64) -> B32 {
    sha256(&[
        NODE_PREFIX,
        left_h,
        &int_to_bytes(left_v),
        right_h,
        &int_to_bytes(right_v),
    ])
}

/// A sibling `(hash, value_msat)` pair on a Merkle branch.
pub type Sibling = (B32, u64);

/// Reconstruct the tree root from a leaf and its Merkle branch (leaf→root
/// order). Returns `(root_hash, root_value_sats)`; errors if the total msat is
/// not a whole number of sats (an invariant the notary asserts).
pub fn compute_root(
    leaf_h: &B32,
    leaf_value_msat: u64,
    merkle_hashes: &[Sibling],
    merkle_index: u64,
) -> Result<(B32, u64)> {
    let mut h = *leaf_h;
    let mut v = leaf_value_msat;
    let mut j = merkle_index;
    for (sib_h, sib_v) in merkle_hashes {
        if j.is_multiple_of(2) {
            // current node is the LEFT child
            h = node_hash(&h, v, sib_h, *sib_v);
        } else {
            // current node is the RIGHT child
            h = node_hash(sib_h, *sib_v, &h, v);
        }
        v = v
            .checked_add(*sib_v)
            .ok_or_else(|| Error::Burn("merkle value overflow".into()))?;
        j >>= 1;
    }
    if !v.is_multiple_of(1000) {
        return Err(Error::Burn(format!("root msat {v} is not whole sats")));
    }
    Ok((h, v / 1000))
}

/// A proof that one Nostr `event_id` contributed `leaf_value` msat to an
/// on-chain burn (`docs/proof-of-burn-api.md` §4.2). Verified independently in
/// [`super::verify`]; never trust it as-is.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BurnProof {
    /// Proof format version (currently `0`).
    pub version: u32,
    /// Reversed genesis hash of the chain; `None`/ignored for mainnet.
    pub chain: Option<String>,
    /// The notarized Nostr event id (raw 32 bytes).
    pub event_id: B32,
    /// This event's burnt share, in **millisatoshis**.
    pub leaf_value_msat: u64,
    /// Nonce used in the leaf hash.
    pub nonce: B32,
    /// Sibling `(hash, value_msat)` pairs, leaf→root order.
    pub merkle_hashes: Vec<Sibling>,
    /// Leaf position in the tree (drives left/right hashing per level).
    pub merkle_index: u64,
    /// Notarization transaction id, **display** hex (reversed SHA256d), as used
    /// by Electrum and the kind-30021 event.
    pub txid: String,
    /// Confirmed block height, or `0` if still unconfirmed (mempool).
    pub block_height: u64,
    /// Optional x-only upvoter key claiming authorship of the upvote.
    pub upvoter_pubkey: Option<B32>,
    /// Optional BIP340 signature over `leaf_hash` by `upvoter_pubkey`.
    pub upvoter_signature: Option<[u8; 64]>,
}

impl BurnProof {
    /// The leaf hash this proof reconstructs (with its declared upvoter key).
    pub fn leaf_hash(&self) -> B32 {
        leaf_hash(
            &self.event_id,
            self.leaf_value_msat,
            &self.nonce,
            self.upvoter_pubkey.as_ref(),
        )
    }

    /// `true` once the proof is confirmed in a block (vs mempool-only).
    pub fn is_confirmed(&self) -> bool {
        self.block_height > 0
    }

    /// Pack `merkle_hashes` into the kind-30021 `n`-tag CSV form
    /// (`"<hash_hex>:<value_msat>,..."`).
    pub fn merkle_hashes_csv(&self) -> String {
        self.merkle_hashes
            .iter()
            .map(|(h, v)| format!("{}:{}", to_hex(h), v))
            .collect::<Vec<_>>()
            .join(",")
    }

    /// Parse the `n`-tag CSV form back into sibling pairs.
    pub fn parse_merkle_hashes_csv(csv: &str) -> Result<Vec<Sibling>> {
        if csv.is_empty() {
            return Ok(Vec::new());
        }
        csv.split(',')
            .map(|pair| {
                let (h, v) = pair
                    .split_once(':')
                    .ok_or_else(|| Error::Burn(format!("bad merkle pair {pair:?}")))?;
                let hash = from_hex_array::<32>(h)?;
                let value: u64 = v
                    .parse()
                    .map_err(|_| Error::Burn(format!("bad merkle value {v:?}")))?;
                Ok((hash, value))
            })
            .collect()
    }

    /// The six packed values of the kind-30021 `n` tag (after the `"n"` kind):
    /// `[txid, block_height, nonce, leaf_value_msat, merkle_index, merkle_csv]`.
    pub fn pack_n_tag(&self) -> [String; 6] {
        [
            self.txid.clone(),
            self.block_height.to_string(),
            to_hex(&self.nonce),
            self.leaf_value_msat.to_string(),
            self.merkle_index.to_string(),
            self.merkle_hashes_csv(),
        ]
    }
}

/// Build a [`BurnProof`] from the pieces a kind-30021 event carries (see
/// [`crate::protocol`] for the event-tag layer). `event_id_hex` is the upvoted
/// Nostr event id; the `n` tag supplies the rest.
#[allow(clippy::too_many_arguments)]
pub fn proof_from_parts(
    version: u32,
    chain: Option<String>,
    event_id_hex: &str,
    txid: &str,
    block_height: u64,
    nonce_hex: &str,
    leaf_value_msat: u64,
    merkle_index: u64,
    merkle_csv: &str,
    upvoter: Option<(&str, &str)>,
) -> Result<BurnProof> {
    let upvoter = upvoter
        .map(|(pk, sig)| -> Result<(B32, [u8; 64])> {
            Ok((from_hex_array::<32>(pk)?, from_hex_array::<64>(sig)?))
        })
        .transpose()?;
    Ok(BurnProof {
        version,
        chain,
        event_id: from_hex_array::<32>(event_id_hex)?,
        leaf_value_msat,
        nonce: from_hex_array::<32>(nonce_hex)?,
        merkle_hashes: BurnProof::parse_merkle_hashes_csv(merkle_csv)?,
        merkle_index,
        txid: txid.to_string(),
        block_height,
        upvoter_pubkey: upvoter.map(|(pk, _)| pk),
        upvoter_signature: upvoter.map(|(_, sig)| sig),
    })
}

/// Parse the JSON object the notary's `get_proof` returns into a [`BurnProof`]
/// (`docs/proof-of-burn-api.md` §4.2 / §5.5).
pub fn proof_from_json(v: &serde_json::Value) -> Result<BurnProof> {
    let get = |k: &str| v.get(k);
    let str_of = |k: &str| -> Result<&str> {
        get(k)
            .and_then(|x| x.as_str())
            .ok_or_else(|| Error::Burn(format!("proof missing string `{k}`")))
    };
    let u64_of = |k: &str| -> Result<u64> {
        get(k)
            .and_then(|x| x.as_u64())
            .ok_or_else(|| Error::Burn(format!("proof missing int `{k}`")))
    };
    let merkle_csv = match get("merkle_hashes") {
        // Either a JSON array of "hash:value" strings, or a pre-joined CSV.
        Some(serde_json::Value::Array(a)) => a
            .iter()
            .filter_map(|x| x.as_str())
            .collect::<Vec<_>>()
            .join(","),
        Some(serde_json::Value::String(s)) => s.clone(),
        _ => String::new(),
    };
    let upvoter = match (get("upvoter_pubkey"), get("upvoter_signature")) {
        (Some(pk), Some(sig)) => match (pk.as_str(), sig.as_str()) {
            (Some(pk), Some(sig)) if !pk.is_empty() && !sig.is_empty() => Some((pk, sig)),
            _ => None,
        },
        _ => None,
    };
    proof_from_parts(
        u64_of("version").unwrap_or(PROOF_VERSION as u64) as u32,
        get("chain").and_then(|x| x.as_str()).map(str::to_string),
        str_of("event_id")?,
        str_of("txid")?,
        u64_of("block_height").unwrap_or(0),
        str_of("nonce")?,
        u64_of("leaf_value")?,
        u64_of("merkle_index")?,
        &merkle_csv,
        upvoter,
    )
}

// ---- a test-only Merkle-sum tree (the notary side, for round-trip tests) ----

#[cfg(test)]
pub(crate) mod testtree {
    //! A minimal notary-side Merkle-sum tree so tests can mint valid proofs and
    //! the on-chain commitments that match them, without a network notary.

    use super::*;

    /// Builds the padded tree and yields a proof for any leaf.
    pub struct TestTree {
        levels: Vec<Vec<(B32, u64)>>, // levels[0] = padded leaves, last = [root]
    }

    impl TestTree {
        /// Build from real `(leaf_hash, value_msat)` leaves, padding up to a
        /// power of two with `(0x00*32, 0)`.
        pub fn new(mut leaves: Vec<(B32, u64)>) -> Self {
            let n = leaves.len().max(1).next_power_of_two();
            leaves.resize(n, ([0u8; 32], 0));
            let mut levels = vec![leaves];
            while levels.last().unwrap().len() > 1 {
                let cur = levels.last().unwrap();
                let mut next = Vec::with_capacity(cur.len() / 2);
                for pair in cur.chunks(2) {
                    let (lh, lv) = pair[0];
                    let (rh, rv) = pair[1];
                    next.push((node_hash(&lh, lv, &rh, rv), lv + rv));
                }
                levels.push(next);
            }
            TestTree { levels }
        }

        /// `(root_hash, root_value_msat)` — note the value is in **msat** (the
        /// tree's native unit); divide by 1000 for the on-chain sats.
        pub fn root(&self) -> (B32, u64) {
            self.levels.last().unwrap()[0]
        }

        /// `(merkle_hashes, merkle_index)` for the leaf at `index`.
        pub fn branch(&self, index: usize) -> (Vec<Sibling>, u64) {
            let mut sibs = Vec::new();
            let mut idx = index;
            for level in &self.levels[..self.levels.len() - 1] {
                let sib = idx ^ 1;
                sibs.push(level[sib]);
                idx >>= 1;
            }
            (sibs, index as u64)
        }
    }

    /// Mint a confirmed [`BurnProof`] for `event_id`/`value_sats`, padding the
    /// batch with `others` dummy leaves so the tree has real siblings. Returns
    /// the proof and the tree root `(hash, sats)` to commit on-chain.
    pub fn mint_proof(
        event_id: B32,
        value_sats: u64,
        nonce: B32,
        upvoter: Option<B32>,
        others: &[(B32, u64)],
        txid: &str,
        block_height: u64,
    ) -> (BurnProof, (B32, u64)) {
        let leaf_value_msat = value_sats * 1000;
        let lh = leaf_hash(&event_id, leaf_value_msat, &nonce, upvoter.as_ref());
        let mut leaves = vec![(lh, leaf_value_msat)];
        leaves.extend_from_slice(others);
        let tree = TestTree::new(leaves);
        let (merkle_hashes, merkle_index) = tree.branch(0);
        let (root_hash, root_msat) = tree.root();
        let proof = BurnProof {
            version: PROOF_VERSION,
            chain: None,
            event_id,
            leaf_value_msat,
            nonce,
            merkle_hashes,
            merkle_index,
            txid: txid.to_string(),
            block_height,
            upvoter_pubkey: upvoter,
            upvoter_signature: None,
        };
        // Return the root value in on-chain **sats** (what the burn output pays).
        (proof, (root_hash, root_msat / 1000))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn int_to_bytes_is_8_byte_be() {
        assert_eq!(int_to_bytes(1), [0, 0, 0, 0, 0, 0, 0, 1]);
        assert_eq!(int_to_bytes(0x0102_0304_0506_0708), [1, 2, 3, 4, 5, 6, 7, 8]);
    }

    #[test]
    fn leaf_hash_is_deterministic_and_pubkey_sensitive() {
        let eid = [0x11u8; 32];
        let nonce = [0x22u8; 32];
        let anon = leaf_hash(&eid, 42_000, &nonce, None);
        let anon2 = leaf_hash(&eid, 42_000, &nonce, None);
        assert_eq!(anon, anon2);
        // A signed leaf (non-zero pubkey) differs from the anonymous one.
        let pk = [0x33u8; 32];
        let signed = leaf_hash(&eid, 42_000, &nonce, Some(&pk));
        assert_ne!(anon, signed);
        // Value and nonce both matter.
        assert_ne!(anon, leaf_hash(&eid, 43_000, &nonce, None));
        assert_ne!(anon, leaf_hash(&eid, 42_000, &[0x99u8; 32], None));
    }

    #[test]
    fn anonymous_leaf_equals_explicit_zero_pubkey() {
        let eid = [7u8; 32];
        let nonce = [9u8; 32];
        let zeros = [0u8; 32];
        assert_eq!(
            leaf_hash(&eid, 1000, &nonce, None),
            leaf_hash(&eid, 1000, &nonce, Some(&zeros))
        );
    }

    #[test]
    fn compute_root_round_trips_through_the_tree() {
        // Four real leaves of varying value; prove each and rebuild the root.
        let leaves: Vec<(B32, u64)> = (0..4)
            .map(|i| (leaf_hash(&[i as u8; 32], (i + 1) * 1000, &[i as u8; 32], None), (i + 1) * 1000))
            .collect();
        let tree = testtree::TestTree::new(leaves.clone());
        let (want_root, want_msat) = tree.root();
        for (i, (lh, lv)) in leaves.iter().enumerate() {
            let (sibs, idx) = tree.branch(i);
            let (got_root, got_sats) = compute_root(lh, *lv, &sibs, idx).unwrap();
            assert_eq!(got_root, want_root, "leaf {i}");
            assert_eq!(got_sats, want_msat / 1000);
        }
        // Total is 1000+2000+3000+4000 = 10000 msat = 10 sat.
        assert_eq!(want_msat / 1000, 10);
    }

    #[test]
    fn compute_root_rejects_fractional_sats() {
        let lh = leaf_hash(&[1u8; 32], 1500, &[2u8; 32], None);
        // A lone leaf of 1500 msat is not a whole number of sats.
        assert!(compute_root(&lh, 1500, &[], 0).is_err());
    }

    #[test]
    fn merkle_csv_round_trips() {
        let (proof, _root) = testtree::mint_proof(
            [0xabu8; 32],
            5,
            [0xcdu8; 32],
            None,
            &[([1u8; 32], 3000), ([2u8; 32], 2000)],
            "00".repeat(32).as_str(),
            10,
        );
        let csv = proof.merkle_hashes_csv();
        let parsed = BurnProof::parse_merkle_hashes_csv(&csv).unwrap();
        assert_eq!(parsed, proof.merkle_hashes);
    }

    #[test]
    fn proof_from_json_parses_a_notary_shaped_object() {
        let (proof, _root) =
            testtree::mint_proof([0x01u8; 32], 7, [0x02u8; 32], None, &[([3u8; 32], 3000)], "ab".repeat(32).as_str(), 0);
        let json = serde_json::json!({
            "version": 0,
            "event_id": to_hex(&proof.event_id),
            "txid": proof.txid,
            "block_height": 0,
            "nonce": to_hex(&proof.nonce),
            "leaf_value": proof.leaf_value_msat,
            "merkle_index": proof.merkle_index,
            "merkle_hashes": proof.merkle_hashes
                .iter().map(|(h,v)| format!("{}:{}", to_hex(h), v)).collect::<Vec<_>>(),
        });
        let parsed = proof_from_json(&json).unwrap();
        assert_eq!(parsed, proof);
    }
}
