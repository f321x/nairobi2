# Nostr Proof‑of‑Burn — Protocol Documentation

**Implementation reference:** [`spesmilo/notary`](https://github.com/spesmilo/notary) (Electrum plugin)
**Whitepaper:** *The Price of Attention: Attaching Bitcoin Fees to Nostr Events*, T. Voegtlin, 2025
**Public notary instance:** `https://notary.electrum.org`
**Scope of this document:** everything a third‑party client needs to (A) request notarization of a Nostr event through the notary's HTTP API, and (B) verify a proof‑of‑burn independently against Electrum indexing servers, without trusting the notary.

This document is written against the actual plugin source (`plugin/notary.py`, `plugin/server.py`, `plugin/__init__.py`, `sign_request.py`, `example.sh`). Where the bundled `plugin/README.md` examples differ from the running code, this document follows the code and flags the difference.

---

## 1. Overview

A **proof‑of‑burn** is a publicly verifiable statement that a specific Nostr `event_id` was committed to the Bitcoin blockchain inside a transaction that irreversibly sacrifices a given number of satoshis to the miners. Clients and relays use the per‑event burnt amount as a spam score: each participant sets a threshold below which they ignore an event.

The notary batches many requests into one Bitcoin transaction using a **Merkle‑sum tree**. The transaction:

- sends the *sum* of all batched amounts to an anyone‑can‑spend output locked by a relative timelock (`OP_CHECKSEQUENCEVERIFY`), so that miners (and only miners, in practice) can claim it — this is the "burn";
- commits the Merkle root of the batch in an `OP_RETURN` output.

A proof for one event is a Merkle branch from that event's leaf up to the root, plus the data needed to locate and validate the transaction on‑chain. Verifying a proof requires **no cooperation from the notary** — only a Bitcoin view, which a client obtains from Electrum indexing servers.

### Roles

| Role | Responsibility |
|---|---|
| **Upvoter / client** | Chooses an `event_id` and an amount to burn, pays the notary's Lightning invoice, retrieves and (independently) verifies the proof. May optionally sign the leaf to claim authorship of the upvote. |
| **Notary** | Collects requests, builds the Merkle‑sum tree, broadcasts the burn transaction, returns proofs, and publishes them as Nostr upvoting events. Trusted only for liveness and for actually burning the funds; **never** trusted for proof validity (that is checked on‑chain). |
| **Relay / consumer** | Reads upvoting events (kind `30021`), verifies them, sums the per‑event burnt amounts, and filters/relays content against a threshold. |
| **Electrum server** | An ElectrumX / electrs / Fulcrum indexing server speaking the Electrum protocol. Used by clients to fetch transactions, confirmations, and SPV Merkle proofs. |

---

## 2. Canonical primitives and serialization

Everything in Parts A and B depends on these definitions. Implement them exactly; a single byte difference breaks every hash.

### 2.1 Units — read this first

Amounts circulate inside the tree in **millisatoshis (msat)**. Only the *root* (and therefore the on‑chain burn output) is expressed in **satoshis**.

| Quantity | Unit | Notes |
|---|---|---|
| `value_sats` (API input to `add_request`) | **sat** | what the user asks to burn |
| `leaf_value` (in the proof, and in the leaf hash) | **msat** | equals `value_sats × 1000` |
| `value` suffix in each `merkle_hashes` entry (`"<hash>:<value>"`) | **msat** | sibling node sums |
| burn output value / root value | **sat** | equals `(sum of all leaf msats) ÷ 1000` |

The tree code asserts `root_msat % 1000 == 0`, i.e. the total batch always rounds to a whole number of satoshis. When you recompute a leaf hash (for signing or verifying), you **must** use the millisatoshi value. When you compare against the on‑chain output, you divide the root msat sum by 1000.

### 2.2 Integer encoding

```
int_to_bytes(x) = x encoded as 8 bytes, big-endian      # 8-byte uint64 BE
```

All node/leaf values inside hashes use this 8‑byte big‑endian encoding (in msat).

### 2.3 Leaf hash

```
H = SHA-256

leaf_hash(event_id, leaf_value_msat, nonce, upvoter_pubkey) =
    H( b"Leaf:"                       # 5 ASCII bytes, literally "Leaf:"
       || event_id                    # 32 bytes
       || int_to_bytes(leaf_value)    # 8 bytes BE, value in MILLISATS
       || nonce                       # 32 bytes
       || (upvoter_pubkey or 0x00*32) # 32 bytes x-only pubkey, or 32 zero bytes if anonymous
     )
```

- `nonce` is a 32‑byte random value chosen by the requesting client. It makes the leaf unique (so identical upvotes are distinguishable and not collapsible).
- `upvoter_pubkey` is a 32‑byte **x‑only** key (BIP340). If the upvote is anonymous, 32 zero bytes are used and no signature is required.

### 2.4 Node hash (inner nodes of the Merkle‑sum tree)

```
node_hash(left_h, left_v, right_h, right_v) =
    H( b"Node:"                       # 5 ASCII bytes, literally "Node:"
       || left_h                      # 32 bytes
       || int_to_bytes(left_v)        # 8 bytes BE (msat)
       || right_h                     # 32 bytes
       || int_to_bytes(right_v)       # 8 bytes BE (msat)
     )

node_value = left_v + right_v          # msat
```

The "Node prefix" and "Leaf prefix" from the whitepaper are the literal ASCII strings `Node:` and `Leaf:`.

### 2.5 Upvoter signature

`upvoter_signature` is a **BIP340 Schnorr** signature whose message is the raw 32‑byte `leaf_hash` (not re‑hashed), produced by the private key matching `upvoter_pubkey`. Verification reconstructs the full point as even‑Y (`0x02 || x`) and runs `schnorr_verify(signature, leaf_hash)`. Anonymous upvotes omit both `upvoter_pubkey` and `upvoter_signature`.

---

## 3. On‑chain notarization transaction

A notarization transaction is a standard Bitcoin transaction (RBF‑enabled) with at least:

1. **One burn output** (P2WSH, value = root value in sats).
2. **One `OP_RETURN` output** (value 0) carrying the magic prefix, Merkle root, and CSV delay.
3. A change output (and a single funding input; the notary chains transactions via RBF to add events to the current tree until it confirms).

The verifier never assumes output ordering — it scans for the `OP_RETURN`, derives the expected burn `scriptPubKey` from the CSV delay it reads there, and then locates the matching output.

### 3.1 Burn output (the sacrifice)

Witness script (redeem script):

```
<csv_delay> OP_CHECKSEQUENCEVERIFY OP_DROP OP_TRUE
```

Byte layout:

```
redeemScript = <push csv_delay>      # CScriptNum, minimally encoded (see note)
             || 0xb2                  # OP_CHECKSEQUENCEVERIFY
             || 0x75                  # OP_DROP
             || 0x51                  # OP_TRUE

scriptPubKey = 0x00 0x20 || SHA-256(redeemScript)   # P2WSH (witness v0, 32-byte program)
```

**CScriptNum note.** `csv_delay` is pushed as a minimally encoded script number (little‑endian, sign bit). For the default `csv_delay = 144` this is the 2‑byte push `0x90 0x00` (the trailing `0x00` keeps the value positive because `0x90` has its high bit set), i.e. the script fragment is `02 90 00 b2 75 51`. Implement CScriptNum correctly or your reconstructed `scriptPubKey` will not match.

After `csv_delay` confirmations the output becomes spendable by anyone (the script leaves `OP_TRUE` on the stack), with the spending input setting `nSequence = csv_delay`. In practice miners sweep it first, which is why sending here is economically equivalent to a sacrifice to miners (per BIP65's rationale, but using CSV not CLTV — CLTV cannot retrospectively prove the miner did not self‑deal in the same block). Using a dedicated P2WSH output forces the burn amount to be ≥ the P2WSH dust limit.

### 3.2 `OP_RETURN` output (the commitment)

```
data = MAGIC || root_hash || csv_delay
     = 0x00 0x21          # MAGIC_BYTES (2 bytes)
     || <32 bytes>        # Merkle root
     || <2 bytes BE>      # csv_delay, unsigned 16-bit big-endian

scriptPubKey = 0x6a 0x24 || data      # OP_RETURN, push of 36 bytes
```

The CSV delay is embedded here so miners can reconstruct the redeem script and claim the burn without any off‑chain data, and so a verifier can bind the `OP_RETURN` to the correct burn output. A valid notarization transaction has exactly one such `OP_RETURN` and one matching burn output; the verifier MUST check the CSV value in the `OP_RETURN` matches the `scriptPubKey` of the burn output.

---

## 4. Merkle‑sum tree and proof structure

### 4.1 Tree construction (notary side, for reference)

- Leaves are `(leaf_hash, value_msat)` pairs, one per request, sorted by value.
- The tree is padded to a power of two with `(0x00*32, 0)` leaves.
- A **subsidy** dummy leaf is added when the batch total is below the P2WSH dust limit, padding the root up to the dust minimum so the burn output is standard. The subsidy is a leaf you cannot produce a meaningful upvote from; it only affects the total.
- Each inner node uses `node_hash`/value‑sum as in §2.4. The root value (msat) divided by 1000 is the sats sent to the burn output.

### 4.2 Proof object

A proof for one leaf is the Merkle branch (sibling list) plus its index:

| Proof field | Type | Meaning |
|---|---|---|
| `version` | int | proof format version (currently `0`) |
| `chain` | hex | reversed genesis block hash of the chain. Omit/ignore for mainnet matching; the running code always includes it |
| `event_id` | hex (32 B) | the notarized Nostr event id |
| `leaf_value` | int (**msat**) | this event's burnt share |
| `nonce` | hex (32 B) | nonce used in the leaf hash |
| `merkle_hashes` | list of `"<hash_hex>:<value_msat>"` | sibling `(hash, value)` pairs, leaf→root order |
| `merkle_index` | int | leaf position in the tree (determines left/right hashing at each level) |
| `txid` | hex | notarization transaction id |
| `block_height` | int | confirmed height, or `0` if still unconfirmed (mempool) |
| `upvoter_pubkey` | hex (32 B) | *optional* x‑only key of the upvoter |
| `upvoter_signature` | hex (64 B) | *optional* BIP340 signature of `leaf_hash` |
| `upvoting_event` | nip19 `nevent` | *optional* pointer to the published Nostr upvoting event |

> The bundled `README.md` shows a `csv_delay` field inside the `get_proof` response. The current code does **not** emit `csv_delay` in the proof; it is recovered from the transaction's `OP_RETURN` during verification. Treat `csv_delay` as on‑chain‑derived, not part of the proof.

### 4.3 Root reconstruction from a proof

```
def compute_root(leaf_h, leaf_v_msat, merkle_hashes, merkle_index):
    h, v = leaf_h, leaf_v_msat
    j = merkle_index
    for (sib_h, sib_v) in merkle_hashes:          # leaf -> root order
        if j % 2 == 0:                            # current node is the LEFT child
            h = node_hash(h, v, sib_h, sib_v)
        else:                                     # current node is the RIGHT child
            h = node_hash(sib_h, sib_v, h, v)
        v = v + sib_v                             # msat
        j = j >> 1
    assert v % 1000 == 0
    return h, v // 1000                           # (root_hash, root_value_SATS)
```

The returned `root_hash` must equal the `OP_RETURN` root, and `root_value_sats` must equal the burn output's value.

---

## 5. Part A — Notarizing an event via the notary API

### 5.1 Endpoints

The bundled aiohttp server registers routes under the application root `/r` on `NOTARY_SERVER_PORT` (default **5455**). The public deployment at `notary.electrum.org` proxies these under `/n` (and the Lightning pay server under `/p`). The working `example.sh` targets:

```
BASE = https://notary.electrum.org/n/api
```

| Method & path | Purpose |
|---|---|
| `POST {BASE}/add_request` | Request notarization → returns a Lightning invoice + `rhash` |
| `POST {BASE}/get_proof` | Fetch the proof once the invoice is paid (notary self‑verifies first) |
| `POST {BASE}/verify_proof` | Ask the notary to verify a proof (convenience; you should also verify yourself — Part B) |
| `POST {BASE}/add_zap_request` | NIP‑57‑style entry point: pay a *total* (burn + fee) for an event id |
| `POST /n/request` | HTML form variant; redirects to a status page |
| `GET  /n/get_status?<rhash>` | WebSocket that streams `{waiting}` → proof JSON as the request progresses |

For self‑hosting, substitute your host/port and the `/r` prefix (e.g. `http://localhost:5455/r/api/add_request`).

### 5.2 `add_request`

**Request body (JSON):**

```json
{
  "event_id": "277419e0a32a8e2181f5b29102eb5008c53fec1b6d980d4b33d0a0aaadf44fc2",
  "value_sats": 42,
  "nonce": "4242424242424242424242424242424242424242424242424242424242424242",
  "upvoter_pubkey": "<32-byte x-only hex>",     // optional
  "upvoter_signature": "<64-byte BIP340 hex>"   // optional
}
```

- `value_sats` — integer satoshis to burn for this event.
- `nonce` — 32 random bytes (hex).
- `upvoter_pubkey` / `upvoter_signature` — include both to claim the upvote. The signature is over `leaf_hash(event_id, value_sats×1000, nonce, upvoter_pubkey)` (see §2.3/§2.5). If you include a pubkey, the notary rejects the request unless the signature verifies.

**Response (JSON):**

```json
{
  "invoice": "lnbc...",
  "rhash":   "a5d29d8e...deeada5b"
}
```

- `invoice` — BOLT11 Lightning invoice for `value_sats + notary_fee(value_sats)`.
- `rhash` — the invoice payment hash; this is your **handle** for `get_proof` and the WebSocket status. Keep it.

**Notary fee schedule** (`notary_fee(x)`, x in sats; the invoice charges `x + fee`):

| burn `x` (sat) | fee | total invoice |
|---|---|---|
| `x ≤ 8` | `x` | `2x` |
| `8 < x ≤ 32` | `x // 2` | `1.5x` |
| `32 < x ≤ 256` | `x // 4` | `1.25x` |
| `x > 256` | `x // 8` | `1.125x` |

### 5.3 Signing the leaf (optional authorship)

To attach authorship, compute the leaf hash with the **millisatoshi** value and sign it (BIP340). Reference (`sign_request.py`):

```python
value_msat = value_sats * 1000
leaf_h = sha256(b"Leaf:" + event_id + int_to_bytes(value_msat) + nonce + pubkey_xonly)
signature = schnorr_sign(privkey, leaf_h)        # message = leaf_h (32 bytes, not re-hashed)
# send pubkey_xonly (hex) and signature (hex) in add_request
```

### 5.4 Pay the invoice

Pay `invoice` over Lightning by any means. The public proof becomes available only after payment is confirmed (`PR_PAID`). Until then `get_proof` returns `{"error": "Waiting for payment"}` and the WebSocket emits `{"waiting": true}`.

### 5.5 `get_proof`

**Request:** `{ "rhash": "<rhash from add_request>" }`

**Response:** the proof object of §4.2. The server runs its own `verify_proof` before returning, so a successful response is already on‑chain‑consistent — but you should still verify independently (Part B).

While the batch is unconfirmed, `block_height` is `0` and `txid` points at the current (RBF‑able) mempool transaction. The notary may replace that transaction to add more events, which **changes the txid and invalidates earlier proofs for that batch**. Re‑fetch (or watch the WebSocket / the replaceable upvoting event) until `block_height > 0`.

### 5.6 WebSocket status (optional)

Open `GET /n/get_status?<rhash>` as a WebSocket. Messages:

- `{"waiting": true}` — invoice unpaid.
- `{"error": "..."}` — e.g. waiting/looked‑up failures.
- the full proof JSON once available; the socket closes after the proof confirms (`block_height > 0`).

### 5.7 `add_zap_request` (NIP‑57 style)

For zap integrations where the payer specifies a *total* (burn + fee) rather than a burn target:

**Request:** `{ "amount_msats": <int>, "event_id": "<hex>" }`
The server generates the nonce, derives the burn amount by inverting the fee schedule, and returns the same `{invoice, rhash}` shape. Anonymous only (no upvoter signature path).

### 5.8 End‑to‑end client flow

```
1. nonce        = random 32 bytes
2. (optional)   leaf_h = leaf_hash(event_id, value_sats*1000, nonce, pubkey)
                 sig    = schnorr_sign(privkey, leaf_h)
3. POST add_request {event_id, value_sats, nonce, [pubkey, sig]}  -> {invoice, rhash}
4. Pay `invoice` over Lightning
5. Poll POST get_proof {rhash}  (or use the WebSocket) until no "error"
6. VERIFY the proof yourself against Electrum servers  (Part B)  <-- do not skip
7. (optional) consume/relay the corresponding kind:30021 upvoting event
```

`example.sh` in the repo is a literal `curl` implementation of steps 3–6.

---

## 6. Part B — Verifying a proof against Electrum indexing servers

This is the trust‑minimizing half: given a proof object (from `get_proof`, or extracted from a kind `30021` event — §7), confirm on‑chain that the claimed burn really happened, **without trusting the notary**. The only external dependency is an Electrum server (and, for full SPV, your own validated header chain).

### 6.1 Electrum protocol methods used

A client speaks the Electrum protocol (JSON‑RPC over TCP/TLS) to an ElectrumX / electrs / Fulcrum server. The methods you need:

| Method | Use |
|---|---|
| `blockchain.transaction.get(txid)` | fetch raw transaction hex (works for confirmed and, on most servers, mempool txs) |
| `blockchain.transaction.get(txid, true)` | verbose form; returns decoded tx plus `confirmations` / `blockhash` (convenience, server‑trusted) |
| `blockchain.transaction.get_merkle(txid[, height])` | SPV Merkle branch: `{block_height, merkle, pos}` |
| `blockchain.block.header(height)` | block header at `height`, to check the Merkle branch against the header's `merkle_root` |
| `blockchain.headers.subscribe` / your header chain | confirm the header connects to validated PoW (full SPV) and to compute confirmations as `tip_height − block_height + 1` |
| `blockchain.scripthash.listunspent(scripthash)` | *optional* — check whether the burn output is still unspent (not yet swept) |

The plugin's own verifier uses `network.get_transaction(txid)` (≈ `blockchain.transaction.get`) plus the wallet's tracked height; an independent client replaces the wallet height with `get_merkle` + headers for a real SPV proof.

### 6.2 Verification algorithm

Given proof `P`:

```
1.  CHAIN
    if P.chain present and P.chain != reversed(genesis_hash_of_your_network):
        reject "wrong chain"

2.  RECONSTRUCT LEAF
    upvoter_pubkey = bytes.fromhex(P.upvoter_pubkey) if present else b""
    leaf_h = leaf_hash(P.event_id, P.leaf_value /*msat*/, P.nonce, upvoter_pubkey)
    if upvoter_pubkey:
        require schnorr_verify(P.upvoter_signature, leaf_h, upvoter_pubkey)   // else reject

3.  RECONSTRUCT ROOT  (see §4.3)
    (root_hash, root_value_sats) = compute_root(leaf_h, P.leaf_value, P.merkle_hashes, P.merkle_index)

4.  FETCH TX
    raw = electrum.blockchain.transaction.get(P.txid)
    tx  = decode(raw)
    require txid(tx) == P.txid          // guards against a lying server returning a different tx

5.  PARSE TX
    find the single OP_RETURN output whose data is 36 bytes starting with 0x0021:
        op_return_root  = data[2:34]
        csv_delay       = int(data[34:36], big-endian)
    rebuild redeemScript from csv_delay; compute its P2WSH scriptPubKey  (see §3.1)
    find the output whose scriptPubKey == that P2WSH program  -> burn_output
        (if absent: reject "burn output not found")

6.  BIND COMMITMENT TO BURN
    require op_return_root == root_hash            // else "root mismatch"
    require burn_output.value == root_value_sats   // else "value mismatch"  (sats == sats)

7.  CONFIRM ON-CHAIN INCLUSION  (choose trust level, see §6.3)
    if P.block_height > 0:
        m = electrum.blockchain.transaction.get_merkle(P.txid, P.block_height)
        require m.block_height == P.block_height
        hdr = electrum.blockchain.block.header(P.block_height)
        require merkle_root_from_branch(P.txid, m.merkle, m.pos) == hdr.merkle_root
        require hdr connects to your validated header chain (PoW)
        confirmations = tip_height - P.block_height + 1
    else:
        // unconfirmed: server-trusted mempool acceptance only (no SPV possible)
        confirmations = 0

8.  ACCEPT
    return { event_id: P.event_id,
             leaf_value_msat: P.leaf_value,    // this event's spam score
             burn_output_value_sats: burn_output.value,
             csv_delay, confirmations, root_hash }
```

Step 6 is the crux: it proves your leaf is part of a tree whose root is committed in an `OP_RETURN`, **and** that the transaction actually paid that exact summed amount into the timelocked burn output. The notary cannot inflate any single event's `leaf_value` without inflating the on‑chain burn by the same amount.

### 6.3 Trust levels

- **Full SPV (confirmed proofs).** Step 7's `get_merkle` + `block.header` path, checked against a header chain you validated yourself, makes inclusion trustless up to the security of SPV. Confirmations come from your tip, not the server's word.
- **Mempool acceptance (unconfirmed, `block_height == 0`).** No Merkle proof exists yet; you are trusting the Electrum server that the tx is in its mempool, and trusting that the notary will not RBF‑replace the burn with a self‑pay. The whitepaper recommends treating this as "good enough" only for provisional spam‑filtering, ideally with a cap on how much unconfirmed content you accept.
- **Server honesty.** Even in the confirmed case, a malicious server could withhold data or lie about mempool state. The fix is the same as any SPV wallet: query several independent servers and/or run your own (`electrs`/Fulcrum). Note the notary deliberately uses a local Bitcoin node for exactly this reason.

### 6.4 Important semantics for relays/clients

- The **spam threshold is applied to `leaf_value`** (this event's per‑event burnt share, in msat), *not* to `burn_output_value_sats` (the whole batch's anchor total). Don't confuse the two.
- An event can have **multiple** valid proofs (multiple upvotes / multiple notarizations). To total the burnt amount for an event, collect upvoting events with **distinct leaf hashes** (the `d` tag, §7) and sum their `leaf_value`s; collapse duplicates that share a leaf hash.
- Re‑org / RBF: an unconfirmed proof's `txid` can change. Always re‑validate before trusting, and prefer confirmed proofs for anything durable.

### 6.5 Reference verification (curl, server‑side)

You can cross‑check your own result against the notary (not a substitute for §6.2):

```
curl -s -X POST {BASE}/verify_proof -H 'Content-Type: application/json' -d @proof.json
# code returns: {confirmations, output_value, csv_delay, root_hash}
# (the bundled README shows an older shape {leaf_value, confirmations, total_value})
```

---

## 7. The Nostr upvoting event (kind 30021)

Proofs are published to Nostr as **addressable** events of kind `30021` (`30000 ≤ kind < 40000`). This is how relays and clients discover and count upvotes without contacting the notary. No Nostr protocol change is required.

### 7.1 Event shape

```json
{
  "kind": 30021,
  "created_at": <unix_ts>,
  "content": "",
  "tags": [
    ["e", "<upvoted_event_id_hex>"],
    ["d", "<leaf_hash_hex>"],
    ["version", "0"],
    ["n",
       "<txid_hex>",
       "<block_height>",
       "<nonce_hex>",
       "<leaf_value_msat>",
       "<merkle_index>",
       "<hash:value,hash:value,...>"   // merkle_hashes joined with commas
    ],
    ["u", "<upvoter_pubkey_hex>", "<upvoter_signature_hex>"],   // optional
    ["p", "<upvoted_event_pubkey_hex>"],                        // optional
    ["chain", "<genesis_hash_hex>"]                             // optional
    // ["expiration", "<ts>"]  is added on non-mainnet (test) deployments
  ],
  "pubkey": "<publisher_pubkey>",
  "id": "<event_id>",
  "sig": "<event_sig>"
}
```

Tag semantics (the first value of a single‑letter tag is indexable, so relays can filter):

- `e` — the upvoted Nostr event id.
- `d` — the **leaf hash** (addressable identifier). Two upvoting events with the same `d` describe the same leaf; keep one. Two with different `d` are distinct upvotes and both count.
- `version` — proof version.
- `n` — the packed proof: `[txid, block_height, nonce, leaf_value_msat, merkle_index, merkle_hashes_csv]`. Exactly 6 values (7 tag elements including `"n"`).
- `u` — upvoter x‑only pubkey and BIP340 signature of the leaf hash (present only for signed/authored upvotes).
- `p` — pubkey of the upvoted event's author (optional).
- `chain` — genesis hash (optional; for non‑mainnet).

### 7.2 Parsing tags back into a proof

To reconstruct a proof object from an event (mirrors `parse_tags`):

```
proof = {}
for tag in event.tags:
    if tag[0]=="e"       and len(tag)==2: proof.event_id      = tag[1]
    if tag[0]=="version" and len(tag)==2: proof.version       = tag[1]
    if tag[0]=="n"       and len(tag)==7:
        proof.txid          = tag[1]
        proof.block_height  = int(tag[2])
        proof.nonce         = tag[3]
        proof.leaf_value    = int(tag[4])     # msat
        proof.merkle_index  = int(tag[5])
        proof.merkle_hashes = tag[6].split(",")
    if tag[0]=="chain"   and len(tag)==2: proof.chain         = tag[1]
    if tag[0]=="u"       and len(tag)==3:
        proof.upvoter_pubkey    = tag[1]
        proof.upvoter_signature = tag[2]
if "nonce" not in proof: discard   # malformed
```

Then run the Part B verification (§6.2) on `proof`. The notary's own `retrieve_proofs` job does exactly this: subscribe to kind `30021`, parse, verify on‑chain, count.

### 7.3 Replaceability, authorship, and counting

- Upvoting events are **replaceable**: an unconfirmed notarization may be RBF‑replaced (new `txid`), and re‑orgs can change `block_height`, so the notary updates the event. Always act on the latest replacement for a given `d` (leaf hash).
- The notary typically publishes the event (the upvoter may be offline when the proof needs updating). While published by the notary, the `content` field is **not** the upvoter's — discard it. Only after the upvoter re‑publishes the finalized proof under their own pubkey (with the `u` tag matching) should `content` be attributed to them; relays should prefer the upvoter‑signed version for a given leaf hash.
- Anyone can re‑publish an existing proof under a new Nostr pubkey, and a third party can even replay a published `u` pubkey/signature into a new notarization (same leaf hash). **De‑duplicate by leaf hash** when counting to avoid inflation.
- Relays may aggregate: count verified upvotes per popular event and serve the count to light clients that opt to trust the relay, instead of shipping every proof.

---

## 8. Economic finality — sweeping the burn (context)

Not part of client verification, but it is *why* the amount is "burnt": after `csv_delay` confirmations, the timelocked P2WSH output is spendable by anyone with witness `[redeemScript]` and the spending input's `nSequence = csv_delay`. Miners can and will claim it before anyone else, so the value is effectively sacrificed to them. The plugin exposes a `sweep <txid>` command that builds such a claiming transaction once `confirmations ≥ csv_delay`. A verifier may optionally check via `blockchain.scripthash.listunspent` whether the burn output is still unspent, but spentness does not change the validity of the proof — the commitment is the burn, regardless of who later sweeps it.

---

## 9. Constants and configuration

| Name | Value | Source |
|---|---|---|
| `MAGIC_BYTES` | `0x0021` | `notary.py` |
| `KIND_UPVOTING_EVENT` | `30021` | `notary.py` |
| `PROOF_VERSION` | `0` | `notary.py` |
| Leaf hash prefix | ASCII `"Leaf:"` | `notary.py`, `sign_request.py` |
| Node hash prefix | ASCII `"Node:"` | `notary.py` |
| Value encoding | 8‑byte big‑endian uint, **msat** | `int_to_bytes` |
| `NOTARY_SERVER_PORT` | `5455` (default) | `__init__.py` |
| `NOTARY_FEERATE` | `1000` sat/kvB (default) | `__init__.py` |
| `NOTARY_CSV_DELAY` | `144` (default) | `__init__.py` |
| `MIN_FEE` | P2WSH dust limit | `notary.py` (`DUST_LIMIT_P2WSH`) |
| Public base URL | `https://notary.electrum.org/n/api` | `example.sh` |

---

## 10. Implementation checklist

**Notarization client (Part A)**
- [ ] Generate a 32‑byte random `nonce` per request.
- [ ] (Optional) Compute `leaf_hash` with **msat** value and BIP340‑sign it; send `upvoter_pubkey` + `upvoter_signature`.
- [ ] `POST add_request`; pay the returned BOLT11 `invoice`; keep `rhash`.
- [ ] Poll `get_proof` (or use the WebSocket) until a proof (no `error`) is returned; expect `block_height == 0` until confirmed and a possibly‑changing `txid` during RBF.

**Independent verifier (Part B)**
- [ ] Implement `int_to_bytes` (8‑byte BE), `leaf_hash`, `node_hash` exactly (ASCII prefixes, msat values).
- [ ] Reconstruct the root from `leaf_value` + `merkle_hashes` + `merkle_index`; assert root msat is a whole number of sats.
- [ ] Fetch the tx via `blockchain.transaction.get` and re‑derive its txid to detect a lying server.
- [ ] Parse the `OP_RETURN` (magic `0x0021`, 32‑byte root, 2‑byte BE CSV); rebuild the P2WSH burn `scriptPubKey` (correct CScriptNum push of `csv_delay`) and locate the burn output.
- [ ] Assert `op_return_root == computed_root` and `burn_output.value (sat) == root_value (sat)`.
- [ ] For confirmed proofs, validate inclusion with `get_merkle` + `block.header` against your own header chain; for unconfirmed, treat as server‑trusted and provisional.
- [ ] Verify the upvoter signature if a pubkey is present.
- [ ] Apply your spam threshold to `leaf_value` (msat), and de‑duplicate upvotes by leaf hash (`d` tag) when summing.

---

## 11. Notes on code‑vs‑README discrepancies

- `get_proof` in the running code does **not** include `csv_delay`; the README example does. `csv_delay` is authoritative only from the on‑chain `OP_RETURN`.
- `verify_proof` returns `{confirmations, output_value, csv_delay, root_hash}` in the code; the README shows an older `{leaf_value, confirmations, total_value}` shape. Verify yourself rather than depending on either.
- The cmdline `get_proof` command names its argument `leaf_hash`, but it is used as the request key, which equals the Lightning `rhash`. Over the HTTP API the field is consistently `rhash`.
- Public paths are under `/n` (proxied) on `notary.electrum.org`; the bundled server registers them under `/r` on port 5455. Adjust the base path to your deployment.
