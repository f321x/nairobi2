//! Part B — verify a [`BurnProof`] against its on-chain burn, **without trusting
//! the notary**. The pure half lives here: given the proof and the raw
//! notarization transaction bytes, prove the leaf is part of a tree whose root
//! is committed in an `OP_RETURN` *and* that the transaction paid that exact
//! summed amount into the timelocked burn output. The network half — fetching
//! the tx and the SPV Merkle branch — is [`super::electrum`].

use nostr_sdk::hashes::{sha256d, Hash};
use nostr_sdk::secp256k1::{schnorr::Signature, Message, XOnlyPublicKey};

use super::proof::{compute_root, BurnProof, B32};
use super::tx::{find_commitment, parse_tx};
use super::{from_hex, to_hex};
use crate::error::{Error, Result};

/// Bitcoin mainnet genesis block hash, **display** order.
const MAINNET_GENESIS_DISPLAY: &str =
    "000000000019d6689c085ae165831e934ff763ae46a2a6c172b3f1b60a8ce26f";

/// The accepted on-chain result of verifying one proof.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VerifiedBurn {
    /// The notarized Nostr event id (raw bytes).
    pub event_id: B32,
    /// This event's burnt share, in msat — the value a spam threshold is
    /// applied to.
    pub leaf_value_msat: u64,
    /// The whole-batch burn output value, in sats (the tree root).
    pub burn_value_sats: u64,
    /// The CSV delay recovered from the `OP_RETURN`.
    pub csv_delay: u16,
    /// Confirmations (`0` when unconfirmed / no tip supplied).
    pub confirmations: u64,
    /// The reconstructed Merkle root (== the committed root).
    pub root_hash: B32,
    /// Whether an upvoter signature was present and verified.
    pub upvoter_verified: bool,
}

/// The `chain` field for mainnet (reversed genesis hash), per the notary.
pub fn mainnet_chain() -> String {
    let mut g = from_hex(MAINNET_GENESIS_DISPLAY).unwrap();
    g.reverse();
    to_hex(&g)
}

/// Verify a proof's BIP340 upvoter signature over its leaf hash, if present.
/// Returns `Ok(true)` when a valid signature was checked, `Ok(false)` when the
/// proof is anonymous, `Err` when a present signature fails.
pub fn verify_upvoter_signature(proof: &BurnProof) -> Result<bool> {
    let (Some(pk), Some(sig)) = (proof.upvoter_pubkey, proof.upvoter_signature) else {
        return Ok(false);
    };
    let xonly =
        XOnlyPublicKey::from_slice(&pk).map_err(|e| Error::Burn(format!("upvoter pubkey: {e}")))?;
    let sig = Signature::from_slice(&sig).map_err(|e| Error::Burn(format!("upvoter sig: {e}")))?;
    let msg = Message::from_digest(proof.leaf_hash());
    nostr_sdk::SECP256K1
        .verify_schnorr(&sig, &msg, &xonly)
        .map_err(|e| Error::Burn(format!("upvoter signature invalid: {e}")))?;
    Ok(true)
}

/// Verify `proof` against the raw notarization transaction `raw_tx`. This is the
/// trust-minimising binding (steps 1–6 + the upvoter signature of
/// `docs/proof-of-burn-api.md` §6.2); SPV inclusion is layered on top by the
/// caller with `tip_height`/Merkle data. `tip_height` (a validated chain tip)
/// turns `block_height` into a confirmation count.
pub fn verify_proof_against_tx(
    proof: &BurnProof,
    raw_tx: &[u8],
    tip_height: Option<u64>,
) -> Result<VerifiedBurn> {
    // 1. CHAIN — reject a proof from another network (testnet/regtest).
    if let Some(chain) = &proof.chain {
        if !chain.is_empty() && chain.eq_ignore_ascii_case(&mainnet_chain()) {
            // mainnet — ok
        } else if !chain.is_empty() {
            return Err(Error::Burn("proof is for a different chain".into()));
        }
    }

    // 2. LEAF + signature.
    let upvoter_verified = verify_upvoter_signature(proof)?;
    let leaf_h = proof.leaf_hash();

    // 3. ROOT from the Merkle branch.
    let (root_hash, root_value_sats) = compute_root(
        &leaf_h,
        proof.leaf_value_msat,
        &proof.merkle_hashes,
        proof.merkle_index,
    )?;

    // 4. FETCH/PARSE TX — and guard against a server returning a different tx.
    let tx = parse_tx(raw_tx)?;
    if !tx.txid.eq_ignore_ascii_case(&proof.txid) {
        return Err(Error::Burn(format!(
            "txid mismatch: tx is {}, proof claims {}",
            tx.txid, proof.txid
        )));
    }

    // 5. PARSE the commitment + locate the burn output.
    let (op_return_root, csv_delay, burn_value_sats) = find_commitment(&tx)?;

    // 6. BIND the commitment to the burn.
    if op_return_root != root_hash {
        return Err(Error::Burn("root mismatch: OP_RETURN ≠ reconstructed root".into()));
    }
    if burn_value_sats != root_value_sats {
        return Err(Error::Burn(format!(
            "value mismatch: burn output {burn_value_sats} sat ≠ root {root_value_sats} sat"
        )));
    }

    // 7. Confirmations (full SPV Merkle inclusion is checked by the caller).
    let confirmations = match (tip_height, proof.block_height) {
        (Some(tip), h) if h > 0 && tip >= h => tip - h + 1,
        _ => 0,
    };

    Ok(VerifiedBurn {
        event_id: proof.event_id,
        leaf_value_msat: proof.leaf_value_msat,
        burn_value_sats,
        csv_delay,
        confirmations,
        root_hash,
        upvoter_verified,
    })
}

/// Recompute a block's Merkle root from a tx's SPV branch
/// (`blockchain.transaction.get_merkle`), to check against the block header.
/// `txid` is display hex; `branch` are display-hex sibling hashes; `pos` is the
/// tx's position in the block.
pub fn merkle_root_from_branch(txid: &str, branch: &[String], pos: u64) -> Result<String> {
    let mut acc = {
        let mut b = from_hex(txid)?;
        b.reverse(); // display → internal
        b
    };
    let mut index = pos;
    for sib_hex in branch {
        let mut sib = from_hex(sib_hex)?;
        sib.reverse();
        let mut buf = Vec::with_capacity(64);
        if index.is_multiple_of(2) {
            buf.extend_from_slice(&acc);
            buf.extend_from_slice(&sib);
        } else {
            buf.extend_from_slice(&sib);
            buf.extend_from_slice(&acc);
        }
        acc = sha256d::Hash::hash(&buf).to_byte_array().to_vec();
        index >>= 1;
    }
    acc.reverse(); // internal → display
    Ok(to_hex(&acc))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::burn::proof::testtree::mint_proof;
    use crate::burn::tx::{burn_script_pubkey, write_var_bytes_for_test};
    use crate::burn::MAGIC_BYTES;

    /// Assemble a synthetic notarization tx committing `root`/`csv` and burning
    /// `sats`, returning its raw bytes and display txid.
    fn synth_tx(root: B32, csv: u16, sats: u64) -> Vec<u8> {
        let mut op_return = vec![0x6a, 0x24];
        op_return.extend_from_slice(&MAGIC_BYTES);
        op_return.extend_from_slice(&root);
        op_return.extend_from_slice(&csv.to_be_bytes());
        let burn_spk = burn_script_pubkey(csv);

        let mut raw = Vec::new();
        raw.extend_from_slice(&2u32.to_le_bytes());
        raw.push(0x01); // legacy, 1 input
        raw.extend_from_slice(&[0u8; 32]);
        raw.extend_from_slice(&0u32.to_le_bytes());
        raw.push(0x00);
        raw.extend_from_slice(&0xffff_ffffu32.to_le_bytes());
        raw.push(0x02);
        raw.extend_from_slice(&sats.to_le_bytes());
        write_var_bytes_for_test(&mut raw, &burn_spk);
        raw.extend_from_slice(&0u64.to_le_bytes());
        write_var_bytes_for_test(&mut raw, &op_return);
        raw.extend_from_slice(&0u32.to_le_bytes());
        raw
    }

    #[test]
    fn verifies_a_minted_proof_against_its_commitment() {
        let nonce = [0x42u8; 32];
        let (mut proof, (root, root_sats)) = mint_proof(
            [0x27u8; 32],
            42,
            nonce,
            None,
            &[([1u8; 32], 8000), ([2u8; 32], 1000)],
            "placeholder",
            900_000,
        );
        let raw = synth_tx(root, 144, root_sats);
        // Fix the proof's txid to the synthetic tx's real txid.
        proof.txid = parse_tx(&raw).unwrap().txid;

        let v = verify_proof_against_tx(&proof, &raw, Some(900_010)).unwrap();
        assert_eq!(v.event_id, proof.event_id);
        assert_eq!(v.leaf_value_msat, 42_000);
        assert_eq!(v.burn_value_sats, root_sats);
        assert_eq!(v.csv_delay, 144);
        assert_eq!(v.confirmations, 11); // 900010 - 900000 + 1
        assert!(!v.upvoter_verified);
    }

    #[test]
    fn rejects_an_inflated_leaf_value() {
        let nonce = [0x42u8; 32];
        let (mut proof, (root, root_sats)) =
            mint_proof([0x27u8; 32], 42, nonce, None, &[([1u8; 32], 9000)], "x", 1);
        let raw = synth_tx(root, 144, root_sats);
        proof.txid = parse_tx(&raw).unwrap().txid;
        // Tamper: claim more than was burnt for this leaf. The root no longer
        // matches the OP_RETURN.
        proof.leaf_value_msat = 1_000_000;
        let err = verify_proof_against_tx(&proof, &raw, None).unwrap_err();
        assert!(matches!(err, Error::Burn(_)));
    }

    #[test]
    fn rejects_a_value_mismatch_on_chain() {
        let nonce = [9u8; 32];
        let (mut proof, (root, root_sats)) =
            mint_proof([5u8; 32], 10, nonce, None, &[([1u8; 32], 6000)], "x", 1);
        // Burn output claims fewer sats than the committed root.
        let raw = synth_tx(root, 144, root_sats - 1);
        proof.txid = parse_tx(&raw).unwrap().txid;
        assert!(verify_proof_against_tx(&proof, &raw, None).is_err());
    }

    #[test]
    fn rejects_txid_mismatch() {
        let (proof, (root, root_sats)) =
            mint_proof([5u8; 32], 10, [9u8; 32], None, &[([1u8; 32], 6000)], "deadbeef", 1);
        let raw = synth_tx(root, 144, root_sats);
        // proof.txid is the bogus "deadbeef…" placeholder, not the real one.
        assert!(verify_proof_against_tx(&proof, &raw, None).is_err());
    }

    #[test]
    fn unconfirmed_proof_has_zero_confirmations() {
        let (mut proof, (root, root_sats)) =
            mint_proof([5u8; 32], 4, [9u8; 32], None, &[([1u8; 32], 6000)], "x", 0);
        let raw = synth_tx(root, 144, root_sats);
        proof.txid = parse_tx(&raw).unwrap().txid;
        let v = verify_proof_against_tx(&proof, &raw, Some(900_000)).unwrap();
        assert_eq!(v.confirmations, 0);
    }

    /// A **real** mempool proof + notarization tx captured live from
    /// `notary.electrum.org` (a kind-30021 event on `relay.damus.io`, batch txid
    /// `4d4e7325…`). This is the only test exercising the full pure pipeline —
    /// leaf hash, root reconstruction, SegWit tx parse (real witness + P2WPKH
    /// change), commitment binding — against bytes the notary actually produced,
    /// not a synthetic fixture. If the notary changes its serialization this is
    /// the canary.
    #[test]
    fn verifies_a_real_notary_mempool_proof() {
        use crate::burn::proof::proof_from_parts;

        // Raw notarization tx (mempool), as `blockchain.transaction.get` returns.
        let raw = from_hex(
            "02000000000101360d2fca24129c62c35257a08e7537cdc9dc153cda863fa7f6b5a225bbdbb14d\
             0200000000fdffffff030000000000000000266a240021b2968b09579c970d9d2116159de0d4a7\
             fa219fd0af90a1e2c43b3b0d0cd382a800904a01000000000000220020f5cf21e2eaf2b5c8945a\
             c0bb7ebf0d25404b249290bc723747c1380d9c02b1bf422d3c0000000000160014d5c2e02a528b\
             63b5ccd0f3777e49bb7eb8c851cd02473044022075821651768cafb39b746b80052dae04bbad68\
             45df2b4edd1b09e70839fdc17c02201ec7a0a7d6ff29110597cd581f99015699d87d04143774d9\
             e26cb6719a8c49b8012102aa493854dfe179162cc606707808f8e611c22ed86913c83d13227b04\
             cca548a1af8f0e00",
        )
        .unwrap();

        let proof = proof_from_parts(
            0,
            Some("6fe28c0ab6f1b372c1a6a246ae63f74f931e8365e15a089c68d6190000000000".into()),
            "dbfc1e8f3ecfbd0a875819e8e210b8bf9f9ba3c2e249b4929375ebfed1568c3b",
            "4d4e732582772f1668071de44398044b1f438c3b3bad2034c0d4ff2dd03d5432",
            0,
            "55fb6a663456d0e85aff17ecf8b3198794a53004aa4baf94d70cb264e0374e1e",
            2000,
            1,
            "7efe03574c17e010b3c8dd6e830c266bfb6caa18c9f7b6f3b94137a58e811d3c:2000,\
             5e95ecea764968048c7556c7106974955f1807c138925f78855e05de02cb687b:10000,\
             e8f67ce51cc581583f14844dd89a8a38272e2f6a65ac69859212e282da604e28:316000",
            None,
        )
        .unwrap();

        // The leaf hash our primitives compute must equal the `d` tag the notary
        // published — a direct cross-check of `leaf_hash` against real bytes.
        assert_eq!(
            to_hex(&proof.leaf_hash()),
            "bb7d3b8968adaa2e4755c73d23aee346d5de9eb23f6def44cb0a17f90c51a20e"
        );

        let v = verify_proof_against_tx(&proof, &raw, None).unwrap();
        assert_eq!(v.leaf_value_msat, 2000);
        assert_eq!(v.burn_value_sats, 330); // P2WSH burn output (vout[1])
        assert_eq!(v.csv_delay, 144);
        assert_eq!(v.confirmations, 0); // mempool
        assert!(!v.upvoter_verified); // anonymous batch leaf
    }
}
