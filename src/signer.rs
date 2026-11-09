//! Agent signer (the optional signed path): turns a Hyperliquid API-wallet
//! private key into signatures over exchange (L1) action envelopes. The key lives ONLY
//! in process memory — never read from .env or disk, never logged.
//!
//! Signature construction is VERIFIED against the official SDKs (fetched live):
//!   - hyperliquid-dex/hyperliquid-rust-sdk @ master:
//!       src/exchange/exchange_client.rs  `Actions::hash`  (msgpack + nonce + vault flag)
//!       src/signature/agent.rs           `l1::Agent`      (EIP-712 domain + struct)
//!       src/signature/create_signature.rs `sign_l1_action` (source "a"/"b")
//!       src/exchange/order.rs + actions.rs (wire field names/order, builder skip-None)
//!   - hyperliquid-dex/hyperliquid-python-sdk @ master: hyperliquid/utils/signing.py
//!       `action_hash` / `construct_phantom_agent` / `l1_payload` (cross-confirms all of it)
//! Construction:
//!   1. connection_id = keccak256( msgpack_named(action) ++ nonce_u64_be8 ++ vault_flag )
//!      vault_flag = 0x00, or 0x01 ++ 20-byte vault address. (msgpack = map with field
//!      NAMES in declaration order; builder/cloid omitted when None.)
//!   2. signature = EIP-712 over struct Agent { string source; bytes32 connectionId; }
//!      in domain { name:"Exchange", version:"1", chainId:1337,
//!      verifyingContract:0x0000…0000 }, source = "a" (mainnet) / "b" (testnet).
//!   3. wire envelope v = 27 + y_parity (both official SDKs).
//! Byte-exactness is proven offline by reproducing the Rust SDK's own pinned signature
//! vectors in tests (test_limit_order_action_hashing / test_tpsl_order_action_hashing
//! from exchange_client.rs): same key + action + nonce => identical r/s/v.
//! Known limitations (documented, honest): expires_after NOT supported (the official
//! Rust SDK omits it too); cloid NOT supported; ApproveBuilderFee/agent approvals are
//! user-signed EIP-712 actions — out of scope here (README guides the user through it).
use alloy_primitives::{keccak256, Address, B256};
use alloy_signer::SignerSync;
use alloy_signer_local::PrivateKeySigner;
use anyhow::{anyhow, bail, Result};

/// Hyperliquid `builder` field: our fee code. f is tenths of a basis point (f=20 => 2 bp).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct HlBuilder {
    /// 0x-prefixed 20-byte builder address (ours; fee lands here).
    pub b: String,
    /// Tenths of bp, 1..=100 enforced by this kit (venue caps 10 bp perps / 100 bp spot).
    pub f: u32,
}

/// Hyperliquid order-type wire variant (SDK `Order`): limit {tif} or trigger.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")] // SDK Order: variants wire as "limit"/"trigger"
pub enum HlOrderType {
    Limit { tif: String },
    Trigger { is_market: bool, trigger_px: String, tpsl: String },
}

/// One order in SDK WIRE form (short field names a/b/p/s/r/t — what the venue's
/// /exchange parser expects; the SDK's OrderRequest uses exactly these).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct HlOrder {
    /// Asset index from /info meta (spot pairs are >= 10_000).
    pub a: u32,
    pub b: bool,
    /// Preformatted decimal string (binary floats never touch the wire).
    pub p: String,
    pub s: String,
    pub r: bool,
    pub t: HlOrderType,
}

impl HlOrder {
    /// True when the asset index names a spot pair (venue: index >= 10_000).
    pub fn is_spot(&self) -> bool {
        self.a >= 10_000
    }
}

/// An L1 order action, SDK declaration order: {"type":"order","orders":[…],
/// "grouping":"na"[,"builder":{b,f}]}. `builder` is OMITTED when None (the official
/// Rust SDK marks it skip_serializing_if — byte-level detail the hash depends on).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct HlOrderAction {
    pub orders: Vec<HlOrder>,
    pub grouping: String,
    pub builder: Option<HlBuilder>,
}

// ---------- hand-rolled MessagePack encoder (typed structs only) ----------
// The official Rust SDK hashes rmp_serde::to_vec_named(action): msgpack maps with
// field-name keys in struct declaration order. We emit exactly that for the shapes
// above; the SDK's pinned signature vectors in tests prove byte-exactness end to end
// (any encoding drift changes the hash and fails the vector).
fn mp_u32(buf: &mut Vec<u8>, v: u32) {
    if v <= 0x7f {
        buf.push(v as u8); // positive fixint
    } else if v <= 0xff {
        buf.push(0xcc); // uint8
        buf.push(v as u8);
    } else if v <= 0xffff {
        buf.push(0xcd); // uint16
        buf.extend_from_slice(&(v as u16).to_be_bytes());
    } else {
        buf.push(0xce); // uint32
        buf.extend_from_slice(&v.to_be_bytes());
    }
}

fn mp_str(buf: &mut Vec<u8>, s: &str) {
    let b = s.as_bytes();
    if b.len() <= 31 {
        buf.push(0xa0 | b.len() as u8); // fixstr
    } else if b.len() <= 255 {
        buf.push(0xd9); // str8
        buf.push(b.len() as u8);
    } else if b.len() <= 65_535 {
        buf.push(0xda); // str16
        buf.extend_from_slice(&(b.len() as u16).to_be_bytes());
    } else {
        buf.push(0xdb); // str32
        buf.extend_from_slice(&(b.len() as u32).to_be_bytes());
    }
    buf.extend_from_slice(b);
}

fn mp_bool(buf: &mut Vec<u8>, v: bool) {
    buf.push(if v { 0xc3 } else { 0xc2 });
}

fn mp_array_header(buf: &mut Vec<u8>, n: usize) -> Result<()> {
    if n <= 15 {
        buf.push(0x90 | n as u8); // fixarray
    } else if n <= 65_535 {
        buf.push(0xdc); // array16
        buf.extend_from_slice(&(n as u16).to_be_bytes());
    } else {
        bail!("orders array too large for msgpack array16: {n}");
    }
    Ok(())
}

fn mp_map_header(buf: &mut Vec<u8>, n: usize) -> Result<()> {
    if n <= 15 {
        buf.push(0x80 | n as u8); // fixmap
    } else if n <= 65_535 {
        buf.push(0xde); // map16
        buf.extend_from_slice(&(n as u16).to_be_bytes());
    } else {
        bail!("action map too large for msgpack map16: {n}");
    }
    Ok(())
}

impl HlOrder {
    fn to_msgpack(&self, buf: &mut Vec<u8>) -> Result<()> {
        mp_map_header(buf, 6)?; // a, b, p, s, r, t (cloid unsupported => always 6)
        mp_str(buf, "a");
        mp_u32(buf, self.a);
        mp_str(buf, "b");
        mp_bool(buf, self.b);
        mp_str(buf, "p");
        mp_str(buf, &self.p);
        mp_str(buf, "s");
        mp_str(buf, &self.s);
        mp_str(buf, "r");
        mp_bool(buf, self.r);
        mp_str(buf, "t");
        match &self.t {
            HlOrderType::Limit { tif } => {
                // SDK Order is an EXTERNALLY-TAGGED enum (order.rs): the wire value of
                // "t" is {"limit":{tif}} / {"trigger":{...}} — a 1-entry wrapper map.
                mp_map_header(buf, 1)?;
                mp_str(buf, "limit");
                mp_map_header(buf, 1)?;
                mp_str(buf, "tif");
                mp_str(buf, tif);
            }
            HlOrderType::Trigger { is_market, trigger_px, tpsl } => {
                mp_map_header(buf, 1)?;
                mp_str(buf, "trigger");
                mp_map_header(buf, 3)?; // SDK Trigger declaration order: isMarket, triggerPx, tpsl
                mp_str(buf, "isMarket");
                mp_bool(buf, *is_market);
                mp_str(buf, "triggerPx");
                mp_str(buf, trigger_px);
                mp_str(buf, "tpsl");
                mp_str(buf, tpsl);
            }
        }
        Ok(())
    }
}

impl HlOrderAction {
    /// msgpack_named(action) — byte-for-byte what rmp_serde::to_vec_named produces for
    /// the SDK's Actions::Order(BulkOrder{…}) (proven by the pinned vector tests).
    pub fn to_msgpack(&self) -> Result<Vec<u8>> {
        if self.orders.is_empty() {
            bail!("refusing to hash an empty order action");
        }
        let mut buf = Vec::with_capacity(128 + self.orders.len() * 64);
        mp_map_header(&mut buf, 3 + self.builder.is_some() as usize)?; // type, orders, grouping [, builder]
        mp_str(&mut buf, "type");
        mp_str(&mut buf, "order");
        mp_str(&mut buf, "orders");
        mp_array_header(&mut buf, self.orders.len())?;
        for o in &self.orders {
            o.to_msgpack(&mut buf)?;
        }
        mp_str(&mut buf, "grouping");
        mp_str(&mut buf, &self.grouping);
        if let Some(b) = &self.builder {
            mp_str(&mut buf, "builder");
            mp_map_header(&mut buf, 2)?; // b, f
            mp_str(&mut buf, "b");
            mp_str(&mut buf, &b.b);
            mp_str(&mut buf, "f");
            mp_u32(&mut buf, b.f);
        }
        Ok(buf)
    }

    /// The JSON body for the POST (key order irrelevant — the venue parses JSON; the
    /// SIGNATURE is over the msgpack form, not this). `builder` is OMITTED exactly
    /// when it is absent from the msgpack form: sending `"builder": null` is a
    /// different action than sending no builder, so the two must never drift.
    pub fn to_json(&self) -> serde_json::Value {
        let mut v = serde_json::json!({
            "type": "order",
            "orders": self.orders,
            "grouping": self.grouping,
        });
        if let Some(b) = &self.builder {
            v["builder"] = serde_json::json!({ "b": b.b, "f": b.f });
        }
        v
    }
}

/// A ready-to-wire ECDSA signature envelope (Hyperliquid `signature` object shape).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct SigEnvelope {
    /// 0x-prefixed, 64 hex chars, zero-padded.
    pub r: String,
    /// 0x-prefixed, 64 hex chars, zero-padded.
    pub s: String,
    /// 27 or 28 (legacy `v` = 27 + y-parity, as both official SDKs send).
    pub v: u8,
}

impl SigEnvelope {
    pub fn to_json(&self) -> serde_json::Value {
        serde_json::json!({ "r": self.r, "s": self.s, "v": self.v })
    }

    /// The 65-byte `r ++ s ++ v` hex string (the SDK's `Signature::to_string()` form).
    pub fn to_65byte_hex(&self) -> String {
        format!("0x{}{}{:02x}", &self.r[2..], &self.s[2..], self.v)
    }
}

// ---------- EIP-712 "Agent" constants (official SDK: signature/agent.rs l1::Agent) ----------
const AGENT_TYPE_STR: &str = "Agent(string source,bytes32 connectionId)";
const DOMAIN_TYPE_STR: &str =
    "EIP712Domain(string name,string version,uint256 chainId,address verifyingContract)";

/// keccak256 of the EIP-712 DOMAIN SEPARATOR for the Exchange domain
/// {name:"Exchange", version:"1", chainId:1337, verifyingContract:0x0}.
///
/// Per EIP-712, domainSeparator = keccak256(typeHash ++ encodeData) — the
/// 0x1901 prefix is NOT part of it; it is applied only when combining the
/// separator with the message struct hash in `agent_signing_hash`. (An earlier
/// revision put 0x1901 here AND there — a double prefix that diverged from the
/// official SDK. Also note addresses ABI-encode as 32-byte words, not 20 bytes;
/// both details are pinned by the SDK vector tests below.)
pub fn exchange_domain_separator() -> B256 {
    let mut buf = Vec::with_capacity(32 * 5);
    buf.extend_from_slice(keccak256(DOMAIN_TYPE_STR.as_bytes()).as_slice());
    buf.extend_from_slice(keccak256("Exchange".as_bytes()).as_slice());
    buf.extend_from_slice(keccak256("1".as_bytes()).as_slice());
    buf.extend_from_slice(&[0u8; 24]); // uint256 chainId: 32-byte BE = 24 zero pad ++ 8-byte value
    buf.extend_from_slice(&1337u64.to_be_bytes());
    // EIP-712 ABI-encodes an address as a 32-byte word (12 bytes of left padding).
    // Appending the raw 20 bytes (previous attempt) changed the PREIMAGE LENGTH and
    // therefore the domain separator, even though Address::ZERO is all zeros —
    // verified against the official SDK create_signature.rs pinned vector.
    buf.extend_from_slice(&[0u8; 12]); // address left-padding to 32B total
    buf.extend_from_slice(Address::ZERO.as_slice()); // verifyingContract, 32B total
    keccak256(&buf)
}

/// keccak256 of the EIP-712 Agent struct hash for a phantom agent.
pub fn agent_struct_hash(source: &str, connection_id: B256) -> B256 {
    let mut buf = Vec::with_capacity(32 + 32 + 32);
    buf.extend_from_slice(keccak256(AGENT_TYPE_STR.as_bytes()).as_slice());
    buf.extend_from_slice(keccak256(source.as_bytes()).as_slice());
    buf.extend_from_slice(connection_id.as_slice());
    keccak256(&buf)
}

/// The final signing hash: keccak(0x1901 ++ domain_separator ++ struct_hash).
pub fn agent_signing_hash(source: &str, connection_id: B256) -> B256 {
    let mut buf = Vec::with_capacity(2 + 32 + 32);
    buf.extend_from_slice(&[0x19, 0x01]);
    buf.extend_from_slice(exchange_domain_separator().as_slice());
    buf.extend_from_slice(agent_struct_hash(source, connection_id).as_slice());
    keccak256(&buf)
}

/// Step 1 of the construction: keccak(msgpack(action) ++ nonce_be8 ++ vault_flag).
pub fn l1_action_hash(action_msgpack: &[u8], nonce: u64, vault: Option<Address>) -> B256 {
    let mut buf = Vec::with_capacity(action_msgpack.len() + 29);
    buf.extend_from_slice(action_msgpack);
    buf.extend_from_slice(&nonce.to_be_bytes());
    match vault {
        None => buf.push(0x00u8),
        Some(addr) => {
            buf.push(0x01u8);
            buf.extend_from_slice(addr.as_slice());
        }
    }
    keccak256(&buf)
}

/// In-memory holder of an API-wallet private key + its sync signer.
/// Clone is intentionally NOT derived: one owner, one key lifetime.
pub struct AgentSigner {
    signer: PrivateKeySigner,
}

impl std::fmt::Debug for AgentSigner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // NEVER print the key or any key-derived bytes beyond the public address.
        f.debug_struct("AgentSigner").field("address", &self.address().to_string()).finish()
    }
}

impl AgentSigner {
    /// Build from a hex private key (with or without 0x). Rejects empty/short/long keys
    /// BEFORE anything else; the parsed key never leaves this struct.
    pub fn from_hex_key(key: &str) -> Result<Self> {
        let trimmed = key.trim();
        if trimmed.is_empty() {
            bail!("empty private key: refuse to construct AgentSigner");
        }
        let hexpart = trimmed.strip_prefix("0x").unwrap_or(trimmed);
        if hexpart.is_empty() {
            bail!("empty private key body after 0x prefix");
        }
        let bytes = alloy_primitives::hex::decode(hexpart)
            .map_err(|e| anyhow!("private key is not valid hex: {e}"))?;
        if bytes.len() != 32 {
            bail!("private key must be exactly 32 bytes, got {}", bytes.len());
        }
        let signer = PrivateKeySigner::from_bytes(&B256::from_slice(&bytes))
            .map_err(|e| anyhow!("invalid private key: {e}"))?;
        Ok(Self { signer })
    }

    /// The public address this key signs for (the API-wallet address).
    pub fn address(&self) -> Address {
        self.signer.address()
    }

    /// Sign an L1 action end to end (VERIFIED construction, see module docs):
    /// hash the msgpack action, wrap it in the EIP-712 Agent phantom agent for
    /// mainnet ("a") or testnet ("b"), sign, return (connection_id, envelope).
    pub fn sign_l1_action(
        &self,
        action_msgpack: &[u8],
        nonce: u64,
        vault: Option<Address>,
        is_mainnet: bool,
    ) -> Result<(B256, SigEnvelope)> {
        let connection_id = l1_action_hash(action_msgpack, nonce, vault);
        let source = if is_mainnet { "a" } else { "b" };
        let hash = agent_signing_hash(source, connection_id);
        let sig = self.signer.sign_hash_sync(&hash)?;
        // wire v = 27 + y-parity (both official SDKs send 27/28); some alloy
        // versions return 0/1 from v_byte(), others 27/28 — normalize both.
        let v = match sig.v_byte() {
            0 | 1 => 27 + sig.v_byte(),
            other => other,
        };
        Ok((
            connection_id,
            SigEnvelope {
                r: format!("0x{:064x}", sig.r()),
                s: format!("0x{:064x}", sig.s()),
                v,
            },
        ))
    }

    /// Recover the signer address from a signed L1 action envelope (verification
    /// helper for tests and audits; the exchange does the equivalent server-side).
    pub fn recover_l1_action(
        connection_id: B256,
        is_mainnet: bool,
        sig: &SigEnvelope,
    ) -> Result<Address> {
        let source = if is_mainnet { "a" } else { "b" };
        let prehash = agent_signing_hash(source, connection_id);
        let r_hex = sig.r.trim_start_matches("0x");
        let s_hex = sig.s.trim_start_matches("0x");
        if r_hex.len() > 64 || s_hex.len() > 64 {
            bail!("signature r/s must be at most 64 hex chars");
        }
        let mut rb = [0u8; 32];
        let mut sb = [0u8; 32];
        alloy_primitives::hex::decode_to_slice(r_hex, &mut rb[32 - r_hex.len() / 2..])
            .map_err(|e| anyhow!("bad signature r hex: {e}"))?;
        alloy_primitives::hex::decode_to_slice(s_hex, &mut sb[32 - s_hex.len() / 2..])
            .map_err(|e| anyhow!("bad signature s hex: {e}"))?;
        let parity = match sig.v {
            27 => false,
            28 => true,
            other => bail!("signature v must be 27 or 28, got {other}"),
        };
        let signature = alloy_primitives::Signature::from_scalars_and_parity(
            B256::from(rb),
            B256::from(sb),
            parity,
        );
        signature
            .recover_address_from_prehash(&prehash)
            .map_err(|e| anyhow!("ecrecover failed: {e}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Fixed test key — generated for tests only, never a real wallet.
    const TEST_KEY: &str = "0x4646464646464646464646464646464646464646464646464646464646464646";

    // ----- OFFICIAL SDK VECTOR (hyperliquid-rust-sdk exchange_client.rs
    // test_limit_order_action_hashing): key e908f8…688e, one limit order
    // {a:1, b:true, p:"2000.0", s:"3.5", r:false, t:{limit:{tif:"Ioc"}}},
    // grouping "na", no builder, nonce 1583838. These assertions prove our
    // msgpack encoding, hash preimage, EIP-712 Agent wrapping and v encoding are
    // byte-identical to the official SDK — any drift fails here.
    const SDK_KEY: &str = "e908f86dbb4d55ac876378565aafeabc187f6690f046459397b17d9b9a19688e";
    const SDK_NONCE: u64 = 1_583_838;
    const SDK_MAINNET_SIG: &str = "0x77957e58e70f43b6b68581f2dc42011fc384538a2e5b7bf42d5b936f19fbb67360721a8598727230f67080efee48c812a6a4442013fd3b0eed509171bef9f23f1c";
    const SDK_TESTNET_SIG: &str = "0xcd0925372ff1ed499e54883e9a6205ecfadec748f80ec463fe2f84f1209648776377961965cb7b12414186b1ea291e95fd512722427efcbcfb3b0b2bcd4d79d01c";

    fn sdk_vector_action() -> HlOrderAction {
        HlOrderAction {
            orders: vec![HlOrder {
                a: 1,
                b: true,
                p: "2000.0".to_string(),
                s: "3.5".to_string(),
                r: false,
                t: HlOrderType::Limit { tif: "Ioc".to_string() },
            }],
            grouping: "na".to_string(),
            builder: None,
        }
    }

    #[test]
    fn sdk_vector_limit_order_mainnet_and_testnet() {
        let signer = AgentSigner::from_hex_key(SDK_KEY).unwrap();
        let mp = sdk_vector_action().to_msgpack().unwrap();
        let (_, sig_main) = signer.sign_l1_action(&mp, SDK_NONCE, None, true).unwrap();
        assert_eq!(
            sig_main.to_65byte_hex(),
            SDK_MAINNET_SIG,
            "mainnet vector mismatch: our construction diverges from the official SDK"
        );
        let (_, sig_test) = signer.sign_l1_action(&mp, SDK_NONCE, None, false).unwrap();
        assert_eq!(
            sig_test.to_65byte_hex(),
            SDK_TESTNET_SIG,
            "testnet vector mismatch: source byte or domain drifted"
        );
        // envelope shape sanity: r/s 0x + 64 hex, v = 27/28
        assert_eq!(sig_main.r.len(), 66);
        assert_eq!(sig_main.s.len(), 66);
        assert!(sig_main.v == 27 || sig_main.v == 28);
    }

    // ----- OFFICIAL SDK VECTOR #2 (test_tpsl_order_action_hashing, tpsl="tp"):
    // proves the Trigger variant's msgpack shape (isMarket/triggerPx/tpsl).
    #[test]
    fn sdk_vector_tpsl_trigger_mainnet() {
        let signer = AgentSigner::from_hex_key(SDK_KEY).unwrap();
        let action = HlOrderAction {
            orders: vec![HlOrder {
                a: 1,
                b: true,
                p: "2000.0".to_string(),
                s: "3.5".to_string(),
                r: false,
                t: HlOrderType::Trigger {
                    is_market: true,
                    trigger_px: "2000.0".to_string(),
                    tpsl: "tp".to_string(),
                },
            }],
            grouping: "na".to_string(),
            builder: None,
        };
        let mp = action.to_msgpack().unwrap();
        let (_, sig) = signer.sign_l1_action(&mp, SDK_NONCE, None, true).unwrap();
        assert_eq!(
            sig.to_65byte_hex(),
            "0xb91e5011dff15e4b4a40753730bda44972132e7b75641f3cac58b66159534a170d422ee1ac3c7a7a2e11e298108a2d6b8da8612caceaeeb3e571de3b2dfda9e41b",
            "trigger/tpsl vector mismatch"
        );
    }

    #[test]
    fn signature_is_deterministic_and_recovers_signer() {
        let a = AgentSigner::from_hex_key(TEST_KEY).unwrap();
        let b = AgentSigner::from_hex_key(TEST_KEY).unwrap();
        let mp = sdk_vector_action().to_msgpack().unwrap();
        let (h1, s1) = a.sign_l1_action(&mp, SDK_NONCE, None, true).unwrap();
        let (h2, s2) = b.sign_l1_action(&mp, SDK_NONCE, None, true).unwrap();
        assert_eq!(h1, h2, "same action+nonce must hash identically");
        assert_eq!(s1, s2, "same key+hash must sign identically (RFC6979 nonce)");
        let recovered = AgentSigner::recover_l1_action(h1, true, &s1).unwrap();
        assert_eq!(recovered, a.address(), "ecrecover must round-trip the signer");
        // a different vault flag must change the hash
        let h_vault = l1_action_hash(&mp, SDK_NONCE, Some(Address::ZERO));
        assert_ne!(h_vault, h1);
    }

    #[test]
    fn msgpack_encoding_is_deterministic_and_builder_changes_bytes() {
        let mut with_builder = sdk_vector_action();
        with_builder.builder = Some(HlBuilder {
            b: "0x1111111111111111111111111111111111111111".to_string(),
            f: 20,
        });
        let mp1 = with_builder.to_msgpack().unwrap();
        let mp2 = with_builder.to_msgpack().unwrap();
        assert_eq!(mp1, mp2, "encoding must be deterministic");
        let plain = sdk_vector_action().to_msgpack().unwrap();
        assert_ne!(mp1, plain, "builder presence must change the encoding");
        // map header: plain action = 3-entry map (0x83), with builder = 4 (0x84)
        assert_eq!(plain[0], 0x83);
        assert_eq!(mp1[0], 0x84);
        // "t" wire shape (externally tagged): {"limit":{"tif":"Ioc"}}
        let probe = HlOrder { a: 1, b: true, p: "1".into(), s: "1".into(), r: false,
            t: HlOrderType::Limit { tif: "Ioc".into() } };
        let mut tb = Vec::new();
        probe.to_msgpack(&mut tb).unwrap();
        // t value = the 6-byte window [0x81, 0xa5, "limit"] somewhere in the stream
        let needle: &[u8] = &[0x81, 0xa5, b'l', b'i', b'm', b'i', b't'];
        assert!(
            tb.windows(needle.len()).any(|w| w == needle),
            "t must encode as {{\"limit\":{{\"tif\":..}}}} (externally tagged), got {:02x?}", &tb[18..]
        );
        // the action starts with the fixstr header (0xa4) then "type" (4 bytes),
        // then the fixstr header for "order" (0xa5), then "order" (5 bytes)
        assert_eq!(plain[1], 0xa4, "fixstr header, len 4");
        assert_eq!(&plain[2..6], b"type");
        assert_eq!(plain[6], 0xa5, "fixstr header, len 5");
        assert_eq!(&plain[7..12], b"order");
    }

    #[test]
    fn rejects_empty_and_malformed_keys() {
        assert!(AgentSigner::from_hex_key("").is_err(), "empty key must be rejected");
        assert!(AgentSigner::from_hex_key("0x").is_err(), "0x-only must be rejected");
        assert!(AgentSigner::from_hex_key("0x1234").is_err(), "short key must be rejected");
        assert!(AgentSigner::from_hex_key("zz").is_err(), "non-hex must be rejected");
        assert!(HlOrderAction { orders: vec![], grouping: "na".into(), builder: None }
            .to_msgpack()
            .is_err(), "empty action must refuse to hash");
    }


}

// ---------- ApproveBuilderFee (the one-time, revocable main-wallet approval) ----------
// Wire truth : the approval is an EXCHANGE L1 action {"type":"approveBuilderFee",
// "maxFeeRate": "<f> tenths of bp as a decimal string", "builder": "0x…"}, signed by the
// USER'S MAIN WALLET — an API/agent wallet CANNOT approve (sign-only by design). One
// approval per builder, max 10 active per user, revocable any time. `maxFeeRate` mirrors
// the SDK's ApproveBuilderFee declaration: a decimal STRING like "0.02%" (2 bp of the
// quote-asset fee). We always emit the exact-percent string form; f tenths-of-bp must be
// a multiple of 10 or the percent has a fractional part the venue parses as a float —
// both forms are legal on the wire, we pick integer tenths to keep the msgpack
// byte-exact with the pinned SDK encoding (strings, declaration order).

/// msgpack for the approveBuilderFee action — rmp_serde::to_vec_named shape for the
/// SDK's Actions::ApproveBuilderFee(ApproveBuilderFee{max_fee_rate, builder}):
/// {"type":"approveBuilderFee","maxFeeRate":<str>,"builder":<str>} in declaration order.
pub fn approve_builder_fee_action_msgpack(builder_addr: &str, fee_tenths_bp: u32) -> Result<Vec<u8>> {
    validate_builder_address(builder_addr)?;
    if fee_tenths_bp == 0 || fee_tenths_bp > 100 {
        bail!(
            "builder fee {fee_tenths_bp} must be 1..=100 tenths of bp (this kit caps at the venue perp limit 10 bp)"
        );
    }
    let max_fee_rate = format!("{}%", fee_tenths_bp as f64 / 1000.0);
    let mut buf = Vec::with_capacity(96);
    mp_map_header(&mut buf, 3)?; // type, maxFeeRate, builder (declaration order)
    mp_str(&mut buf, "type");
    mp_str(&mut buf, "approveBuilderFee");
    mp_str(&mut buf, "maxFeeRate");
    mp_str(&mut buf, &max_fee_rate);
    mp_str(&mut buf, "builder");
    mp_str(&mut buf, &builder_addr.trim().to_ascii_lowercase());
    Ok(buf)
}

pub fn validate_builder_address(builder_addr: &str) -> Result<()> {
    let b = builder_addr.trim().to_ascii_lowercase();
    if !b.starts_with("0x") || b.len() != 42 || !b[2..].chars().all(|c| c.is_ascii_hexdigit()) {
        bail!("builder address must be a 0x-prefixed 20-byte hex address, got {builder_addr:?}");
    }
    Ok(())
}
