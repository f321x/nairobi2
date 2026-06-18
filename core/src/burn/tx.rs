//! A minimal Bitcoin transaction + script parser — only what proof verification
//! needs: compute a transaction's txid, find the `OP_RETURN` commitment, and
//! reconstruct the P2WSH burn output's `scriptPubKey` from a CSV delay. No
//! script execution, no signature checking; we only read outputs and hash.
//!
//! Hand-rolled in the spirit of the crate's geohash/HTTP code, reusing the
//! `bitcoin_hashes` already in the tree (via `nostr_sdk`) for SHA-256(d).

use nostr_sdk::hashes::{sha256, sha256d, Hash};

use super::{to_hex, MAGIC_BYTES};
use crate::burn::proof::B32;
use crate::error::{Error, Result};

/// One transaction output (value in sats + `scriptPubKey`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TxOut {
    pub value_sats: u64,
    pub script_pubkey: Vec<u8>,
}

/// A decoded transaction — just the parts verification needs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Tx {
    /// Display txid (reversed SHA256d), as Electrum and the proof use it.
    pub txid: String,
    pub outputs: Vec<TxOut>,
}

struct Cur<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> Cur<'a> {
    fn new(data: &'a [u8]) -> Self {
        Cur { data, pos: 0 }
    }
    fn need(&self, n: usize) -> Result<()> {
        if self.pos + n > self.data.len() {
            return Err(Error::Burn("tx truncated".into()));
        }
        Ok(())
    }
    fn take(&mut self, n: usize) -> Result<&'a [u8]> {
        self.need(n)?;
        let s = &self.data[self.pos..self.pos + n];
        self.pos += n;
        Ok(s)
    }
    fn u8(&mut self) -> Result<u8> {
        Ok(self.take(1)?[0])
    }
    fn u32_le(&mut self) -> Result<u32> {
        let b = self.take(4)?;
        Ok(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    }
    fn u64_le(&mut self) -> Result<u64> {
        let b = self.take(8)?;
        Ok(u64::from_le_bytes(b.try_into().unwrap()))
    }
    /// Bitcoin CompactSize varint.
    fn varint(&mut self) -> Result<u64> {
        let n = self.u8()?;
        Ok(match n {
            0xfd => self.take(2)?.iter().rev().fold(0u64, |a, &b| (a << 8) | b as u64),
            0xfe => self.u32_le()? as u64,
            0xff => self.u64_le()?,
            x => x as u64,
        })
    }
    /// A length-prefixed byte string (`varint len || bytes`).
    fn var_bytes(&mut self) -> Result<&'a [u8]> {
        let n = self.varint()? as usize;
        self.take(n)
    }
}

/// Encode `len(payload) || payload` as a varint-prefixed string into `out`.
fn write_var_bytes(out: &mut Vec<u8>, payload: &[u8]) {
    write_varint(out, payload.len() as u64);
    out.extend_from_slice(payload);
}

fn write_varint(out: &mut Vec<u8>, n: u64) {
    match n {
        0..=0xfc => out.push(n as u8),
        0xfd..=0xffff => {
            out.push(0xfd);
            out.extend_from_slice(&(n as u16).to_le_bytes());
        }
        0x1_0000..=0xffff_ffff => {
            out.push(0xfe);
            out.extend_from_slice(&(n as u32).to_le_bytes());
        }
        _ => {
            out.push(0xff);
            out.extend_from_slice(&n.to_le_bytes());
        }
    }
}

/// Parse a (possibly SegWit) transaction. The txid is computed over the
/// **legacy** serialization (marker/flag/witness stripped), reversed for
/// display — exactly Bitcoin's rule.
pub fn parse_tx(raw: &[u8]) -> Result<Tx> {
    let mut c = Cur::new(raw);
    let version = c.u32_le()?;

    // SegWit marker (0x00) + flag (non-zero, normally 0x01).
    let mut segwit = false;
    c.need(1)?;
    if c.data[c.pos] == 0x00 {
        c.pos += 1;
        let flag = c.u8()?;
        if flag == 0 {
            return Err(Error::Burn("zero segwit flag".into()));
        }
        segwit = true;
    }

    // Inputs.
    let n_in = c.varint()?;
    let mut legacy_inputs = Vec::new(); // re-serialized for the txid
    for _ in 0..n_in {
        let prevout = c.take(36)?; // txid(32) + vout(4)
        let script_sig = c.var_bytes()?;
        let sequence = c.take(4)?;
        legacy_inputs.push((prevout.to_vec(), script_sig.to_vec(), sequence.to_vec()));
    }

    // Outputs.
    let n_out = c.varint()?;
    let mut outputs = Vec::with_capacity(n_out as usize);
    for _ in 0..n_out {
        let value_sats = c.u64_le()?;
        let script_pubkey = c.var_bytes()?.to_vec();
        outputs.push(TxOut {
            value_sats,
            script_pubkey,
        });
    }

    // Witnesses (skipped; not part of the txid).
    if segwit {
        for _ in 0..n_in {
            let items = c.varint()?;
            for _ in 0..items {
                let _ = c.var_bytes()?;
            }
        }
    }
    let locktime = c.u32_le()?;

    // Re-serialize the legacy form and double-SHA256 it.
    let mut ser = Vec::with_capacity(raw.len());
    ser.extend_from_slice(&version.to_le_bytes());
    write_varint(&mut ser, n_in);
    for (prevout, script_sig, sequence) in &legacy_inputs {
        ser.extend_from_slice(prevout);
        write_var_bytes(&mut ser, script_sig);
        ser.extend_from_slice(sequence);
    }
    write_varint(&mut ser, n_out);
    for o in &outputs {
        ser.extend_from_slice(&o.value_sats.to_le_bytes());
        write_var_bytes(&mut ser, &o.script_pubkey);
    }
    ser.extend_from_slice(&locktime.to_le_bytes());

    let mut txid_bytes = sha256d::Hash::hash(&ser).to_byte_array();
    txid_bytes.reverse(); // internal → display (big-endian)
    Ok(Tx {
        txid: to_hex(&txid_bytes),
        outputs,
    })
}

/// Minimal `CScriptNum` push of a small non-negative integer: the script bytes
/// `len || little-endian-magnitude`, appending `0x00` when the top byte's sign
/// bit is set (so the number stays positive). `csv=144` → `02 90 00`.
pub fn cscriptnum_push(n: u32) -> Vec<u8> {
    if n == 0 {
        return vec![0x00]; // OP_0 / empty push
    }
    let mut le = Vec::new();
    let mut v = n;
    while v > 0 {
        le.push((v & 0xff) as u8);
        v >>= 8;
    }
    if le.last().is_some_and(|b| b & 0x80 != 0) {
        le.push(0x00);
    }
    let mut out = Vec::with_capacity(le.len() + 1);
    out.push(le.len() as u8); // push-length opcode (data ≤ 75 bytes)
    out.extend_from_slice(&le);
    out
}

/// The burn redeem script `<csv> OP_CHECKSEQUENCEVERIFY OP_DROP OP_TRUE`.
pub fn redeem_script(csv_delay: u16) -> Vec<u8> {
    let mut s = cscriptnum_push(csv_delay as u32);
    s.extend_from_slice(&[0xb2, 0x75, 0x51]); // OP_CSV OP_DROP OP_TRUE
    s
}

/// The P2WSH `scriptPubKey` (`0x00 0x20 || SHA256(redeemScript)`) of the burn
/// output for `csv_delay`.
pub fn burn_script_pubkey(csv_delay: u16) -> Vec<u8> {
    let wsh = sha256::Hash::hash(&redeem_script(csv_delay)).to_byte_array();
    let mut spk = Vec::with_capacity(34);
    spk.push(0x00); // witness v0
    spk.push(0x20); // 32-byte program
    spk.extend_from_slice(&wsh);
    spk
}

/// If `script` is the notarization `OP_RETURN`, return `(root_hash, csv_delay)`.
/// Layout: `0x6a 0x24 || MAGIC(2) || root(32) || csv(2 BE)`.
pub fn parse_op_return(script: &[u8]) -> Option<(B32, u16)> {
    if script.len() != 38 || script[0] != 0x6a || script[1] != 0x24 {
        return None;
    }
    let data = &script[2..]; // 36 bytes
    if data[0..2] != MAGIC_BYTES {
        return None;
    }
    let mut root = [0u8; 32];
    root.copy_from_slice(&data[2..34]);
    let csv = u16::from_be_bytes([data[34], data[35]]);
    Some((root, csv))
}

/// Locate the notarization commitment + matching burn output in `tx`. Returns
/// `(op_return_root, csv_delay, burn_value_sats)`. Requires exactly one
/// `OP_RETURN` and a P2WSH output whose script matches the embedded CSV.
pub fn find_commitment(tx: &Tx) -> Result<(B32, u16, u64)> {
    let mut commitment = None;
    for o in &tx.outputs {
        if let Some(found) = parse_op_return(&o.script_pubkey) {
            if commitment.is_some() {
                return Err(Error::Burn("multiple OP_RETURN outputs".into()));
            }
            commitment = Some(found);
        }
    }
    let (root, csv) = commitment.ok_or_else(|| Error::Burn("no OP_RETURN commitment".into()))?;
    let want_spk = burn_script_pubkey(csv);
    let burn = tx
        .outputs
        .iter()
        .find(|o| o.script_pubkey == want_spk)
        .ok_or_else(|| Error::Burn("burn output not found for the committed CSV".into()))?;
    Ok((root, csv, burn.value_sats))
}

/// Test-only re-export of the internal varint-prefixed writer so sibling
/// modules' tests can assemble synthetic transactions.
#[cfg(test)]
pub(crate) fn write_var_bytes_for_test(out: &mut Vec<u8>, payload: &[u8]) {
    write_var_bytes(out, payload);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::burn::from_hex;

    // The Bitcoin genesis coinbase: a canonical (rawhex, txid) vector, so we
    // know our txid computation matches Bitcoin's, not just itself.
    const GENESIS_RAW: &str = "01000000010000000000000000000000000000000000000000000000000000000000000000ffffffff4d04ffff001d0104455468652054696d65732030332f4a616e2f32303039204368616e63656c6c6f72206f6e206272696e6b206f66207365636f6e64206261696c6f757420666f722062616e6b73ffffffff0100f2052a01000000434104678afdb0fe5548271967f1a67130b7105cd6a828e03909a67962e0ea1f61deb649f6bc3f4cef38c4f35504e51ec112de5c384df7ba0b8d578a4c702b6bf11d5fac00000000";
    const GENESIS_TXID: &str = "4a5e1e4baab89f3a32518a88c31bc87f618f76673e2cc77ab2127b7afdeda33b";

    #[test]
    fn computes_canonical_genesis_txid() {
        let tx = parse_tx(&from_hex(GENESIS_RAW).unwrap()).unwrap();
        assert_eq!(tx.txid, GENESIS_TXID);
        assert_eq!(tx.outputs.len(), 1);
        assert_eq!(tx.outputs[0].value_sats, 5_000_000_000);
    }

    #[test]
    fn cscriptnum_matches_documented_csv_144() {
        // docs §3.1: csv=144 pushes 0x90 0x00 → redeemScript 02 90 00 b2 75 51.
        assert_eq!(cscriptnum_push(144), vec![0x02, 0x90, 0x00]);
        assert_eq!(redeem_script(144), vec![0x02, 0x90, 0x00, 0xb2, 0x75, 0x51]);
        // No padding byte when the top byte is already positive.
        assert_eq!(cscriptnum_push(1), vec![0x01, 0x01]);
        assert_eq!(cscriptnum_push(256), vec![0x02, 0x00, 0x01]);
    }

    #[test]
    fn burn_script_pubkey_is_well_formed_p2wsh() {
        let spk = burn_script_pubkey(144);
        assert_eq!(spk.len(), 34);
        assert_eq!(spk[0], 0x00);
        assert_eq!(spk[1], 0x20);
    }

    #[test]
    fn parses_op_return_commitment() {
        let root = [0x55u8; 32];
        let csv: u16 = 144;
        let mut script = vec![0x6a, 0x24];
        script.extend_from_slice(&MAGIC_BYTES);
        script.extend_from_slice(&root);
        script.extend_from_slice(&csv.to_be_bytes());
        assert_eq!(parse_op_return(&script), Some((root, csv)));
        // Wrong magic is rejected.
        let mut bad = script.clone();
        bad[2] = 0xff;
        assert_eq!(parse_op_return(&bad), None);
    }

    /// Build a synthetic notarization tx (SegWit) committing `root`/`csv` and
    /// paying `burn_sats` to the matching P2WSH output, then assert we recover
    /// all three and that the SegWit txid strips the witness correctly.
    #[test]
    fn finds_commitment_in_a_synthetic_segwit_tx() {
        let root = [0xABu8; 32];
        let csv: u16 = 144;
        let burn_sats: u64 = 7;

        let mut op_return = vec![0x6a, 0x24];
        op_return.extend_from_slice(&MAGIC_BYTES);
        op_return.extend_from_slice(&root);
        op_return.extend_from_slice(&csv.to_be_bytes());
        let burn_spk = burn_script_pubkey(csv);

        // version | marker flag | 1 in (empty scriptSig) | 2 outs | witness | locktime
        let mut raw = Vec::new();
        raw.extend_from_slice(&2u32.to_le_bytes());
        raw.extend_from_slice(&[0x00, 0x01]); // segwit marker+flag
        raw.push(0x01); // 1 input
        raw.extend_from_slice(&[0u8; 32]); // prevout txid
        raw.extend_from_slice(&0u32.to_le_bytes()); // vout
        raw.push(0x00); // empty scriptSig
        raw.extend_from_slice(&0xffff_ffffu32.to_le_bytes()); // sequence
        raw.push(0x02); // 2 outputs
        raw.extend_from_slice(&burn_sats.to_le_bytes());
        write_var_bytes(&mut raw, &burn_spk);
        raw.extend_from_slice(&0u64.to_le_bytes()); // OP_RETURN value 0
        write_var_bytes(&mut raw, &op_return);
        // witness: 1 stack item of 4 bytes
        raw.push(0x01);
        write_var_bytes(&mut raw, &[1, 2, 3, 4]);
        raw.extend_from_slice(&0u32.to_le_bytes()); // locktime

        let tx = parse_tx(&raw).unwrap();
        let (got_root, got_csv, got_val) = find_commitment(&tx).unwrap();
        assert_eq!(got_root, root);
        assert_eq!(got_csv, csv);
        assert_eq!(got_val, burn_sats);

        // The txid must equal the legacy serialization's hash (witness stripped).
        let mut legacy = Vec::new();
        legacy.extend_from_slice(&2u32.to_le_bytes());
        legacy.push(0x01);
        legacy.extend_from_slice(&[0u8; 32]);
        legacy.extend_from_slice(&0u32.to_le_bytes());
        legacy.push(0x00);
        legacy.extend_from_slice(&0xffff_ffffu32.to_le_bytes());
        legacy.push(0x02);
        legacy.extend_from_slice(&burn_sats.to_le_bytes());
        write_var_bytes(&mut legacy, &burn_spk);
        legacy.extend_from_slice(&0u64.to_le_bytes());
        write_var_bytes(&mut legacy, &op_return);
        legacy.extend_from_slice(&0u32.to_le_bytes());
        let mut want = sha256d::Hash::hash(&legacy).to_byte_array();
        want.reverse();
        assert_eq!(tx.txid, to_hex(&want));
    }
}
