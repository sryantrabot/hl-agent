//! Exchange client: the OPTIONAL signed path.
//! POSTs ONE signed action to `{base}/exchange` and parses the venue's
//! `{status:"ok"}` / `{status:"err", response:<text>}` envelope. NO retry loop —
//! Hyperliquid's nonce window makes blind replays dangerous (see knowledge/intel/
//! 2026-11-05-agent-pain/hl-builder-codes.md: one API wallet per process, T-2d..T+1d).
//!
//! SAFETY LAWS (dispatch): no live keys in this repo, no mainnet POSTs from tests,
//! nothing read from .env. The default base is TESTNET; MAINNET exists as a const for
//! the operator's explicit opt-in only.
use crate::signer::{AgentSigner, HlBuilder, HlOrder, HlOrderAction, HlOrderType, SigEnvelope};
use anyhow::{anyhow, bail, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const MAINNET: &str = crate::hl::MAINNET;
pub const TESTNET: &str = crate::hl::TESTNET;

/// Wire order fields for the venue: short keys (a/b/p/s/r/t) per the official
/// SDK OrderRequest — the JSON body mirrors the signed msgpack field names.
#[derive(Debug, Clone, Serialize)]
pub struct OrderWire {
    /// Asset index from /info meta (spot pairs are >= 10_000).
    pub a: u32,
    pub b: bool,
    /// Preformatted decimal string (binary floats never touch the wire).
    pub p: String,
    pub s: String,
    pub r: bool,
    /// {"limit":{"tif":"Gtc"|"Ioc"|"Alo"}} or {"market":{}} — kept as raw JSON.
    #[serde(rename = "t")]
    pub order_type: Value,
}

impl OrderWire {
    pub fn limit(a: u32, b: bool, p: &str, s: &str, r: bool, tif: &str) -> Self {
        Self { a, b, p: p.to_string(), s: s.to_string(), r, order_type: serde_json::json!({ "limit": { "tif": tif } }) }
    }
    pub fn market(a: u32, b: bool, s: &str, r: bool) -> Self {
        Self { a, b, p: "0".to_string(), s: s.to_string(), r, order_type: serde_json::json!({ "market": {} }) }
    }
    /// True when the asset index names a spot pair (venue: index >= 10_000).
    pub fn is_spot(&self) -> bool {
        self.a >= 10_000
    }
}

/// Docs-faithful builder-fee validation (knowledge/intel/2026-11-05-agent-pain/
/// hl-builder-codes.md): f is tenths of bp; caps 10 bp (f<=100) enforced conservatively
/// for perps AND spot; builder codes "do not apply to the buying side of spot trades".
pub fn validate_builder_fee(fee_tenths_bp: u32, order: &OrderWire) -> Result<()> {
    if fee_tenths_bp > 100 {
        bail!(
            "builder fee {fee_tenths_bp} exceeds the 100-tenths (10 bp) cap enforced by this kit \
             (venue caps: 10 bp perps, 100 bp spot; we stay under both)"
        );
    }
    if order.is_spot() && order.b {
        bail!(
            "builder fee on the BUY side of spot (asset {}) is not supported by the venue \
             (builder codes apply to quote-asset fees only) — refusing to attach it",
            order.a
        );
    }
    Ok(())
}

/// Parsed venue envelope for an exchange POST.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ExchangeResponse {
    /// "ok" or "err" as sent by the venue ("" when the envelope was malformed).
    #[serde(default)]
    pub status: String,
    /// ok: the response payload; err: the error text. Kept raw either way.
    #[serde(default)]
    pub response: Value,
    /// The full body, verbatim, for the audit trail.
    #[serde(default)]
    pub raw: String,
}

impl ExchangeResponse {
    pub fn is_ok(&self) -> bool {
        self.status == "ok"
    }

    /// The venue error text when status == "err" ("" otherwise).
    pub fn error_text(&self) -> String {
        if self.is_ok() {
            String::new()
        } else {
            match &self.response {
                Value::String(s) => s.clone(),
                other => other.to_string(),
            }
        }
    }
}

/// Blocking signed client over `{base}/exchange`. One HTTP attempt per call.
pub struct ExchangeClient {
    http: reqwest::blocking::Client,
    base: String,
    signer: AgentSigner,
}

impl std::fmt::Debug for ExchangeClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // signer is deliberately opaque (never logs key material)
        f.debug_struct("ExchangeClient").field("base", &self.base).finish_non_exhaustive()
    }
}

impl ExchangeClient {
    pub fn new(base: &str, signer: AgentSigner) -> Self {
        Self {
            http: reqwest::blocking::Client::builder()
                .timeout(std::time::Duration::from_secs(15))
                .build()
                .expect("reqwest client"),
            base: base.trim_end_matches('/').to_string(),
            signer,
        }
    }

    /// TESTNET by default (dispatch law: testnet first).
    pub fn testnet(signer: AgentSigner) -> Self {
        Self::new(TESTNET, signer)
    }

    /// The signer's public address, so the live path can prove the `--user` account it
    /// reads caps from is really the account this key may trade for. Never the key.
    pub fn signer_address(&self) -> String {
        format!("{:#x}", self.signer.address())
    }

    /// Build the wire envelope {action, nonce, signature} — pure, so tests can
    /// assert construction without any network.
    pub fn envelope(action: Value, nonce: u64, sig: &SigEnvelope) -> Value {
        serde_json::json!({
            "action": action,
            "nonce": nonce,
            "signature": sig.to_json(),
        })
    }

    /// Sign `action` (VERIFIED construction: msgpack + EIP-712 Agent, see signer.rs)
    /// and POST it ONCE to {base}/exchange. On a nonce/replay error the error is
    /// RETURNED, never retried (docs warn replay risk).
    pub fn post_l1_action(&self, action: &HlOrderAction, nonce: u64) -> Result<ExchangeResponse> {
        let mp = action.to_msgpack()?;
        let (_, sig) = self.signer.sign_l1_action(&mp, nonce, None, false)?;
        let body = Self::envelope(action.to_json(), nonce, &sig);
        self.post_body(body)
    }

    /// Sign `action` once and POST it once, returning BOTH the signed body (the
    /// user's audit trail) and the parsed venue answer. Used by the live-order
    /// path; no retry, ever (nonce window).
    pub fn sign_and_post_action(
        &self,
        action: &HlOrderAction,
        nonce: u64,
        is_mainnet: bool,
    ) -> Result<(serde_json::Value, ExchangeResponse)> {
        let mp = action.to_msgpack()?;
        let (_, sig) = self.signer.sign_l1_action(&mp, nonce, None, is_mainnet)?;
        let body = Self::envelope(action.to_json(), nonce, &sig);
        let resp = self.post_body(body.clone())?;
        Ok((body, resp))
    }

    /// Place ONE order with our builder code attached. Validates the fee BEFORE
    /// signing; the user's main wallet must have approved the builder
    /// (ApproveBuilderFee, one-time, revocable) or the venue rejects. Mainnet is
    /// the operator's explicit opt-in: ExchangeClient::new(MAINNET, …).
    pub fn place_order_with_builder(
        &self,
        order: &OrderWire,
        builder_addr: &str,
        fee_tenths_bp: u32,
        nonce: u64,
        is_mainnet: bool,
    ) -> Result<ExchangeResponse> {
        validate_builder_fee(fee_tenths_bp, order)?;
        let b = builder_addr.trim().to_ascii_lowercase();
        if !b.starts_with("0x") || b.len() != 42 || !b[2..].chars().all(|c| c.is_ascii_hexdigit()) {
            bail!("builder address must be a 0x-prefixed 20-byte hex address, got {builder_addr:?}");
        }
        let hl_order = Self::wire_to_hl_order(order)?;
        let action = HlOrderAction {
            orders: vec![hl_order],
            grouping: "na".to_string(),
            builder: Some(HlBuilder { b: b, f: fee_tenths_bp }),
        };
        let mp = action.to_msgpack()?;
        let (_, sig) = self.signer.sign_l1_action(&mp, nonce, None, is_mainnet)?;
        let body = Self::envelope(action.to_json(), nonce, &sig);
        self.post_body(body)
    }

    /// Like `sign_and_post_action`, but NEVER throws away the signed body: when the
    /// POST itself fails (timeout, 5xx, connection reset) we cannot know whether the
    /// venue received it, and the caller must tell the user to check open orders
    /// before retrying — a blind retry with a fresh nonce double-fills.
    pub fn sign_and_post_audited(
        &self,
        action: &HlOrderAction,
        nonce: u64,
        is_mainnet: bool,
    ) -> Result<(serde_json::Value, anyhow::Result<ExchangeResponse>)> {
        let mp = action.to_msgpack()?;
        let (_, sig) = self.signer.sign_l1_action(&mp, nonce, None, is_mainnet)?;
        let body = Self::envelope(action.to_json(), nonce, &sig);
        let posted = self.post_body(body.clone());
        Ok((body, posted))
    }

    fn post_body(&self, body: Value) -> Result<ExchangeResponse> {

        let url = format!("{}/exchange", self.base);
        let resp = self.http.post(&url).json(&body).send()?;
        let status = resp.status();
        let text = resp.text()?;
        if !status.is_success() {
            bail!("hyperliquid exchange {} -> HTTP {}: {}", url, status, truncate(&text, 200));
        }
        let parsed: Result<ExchangeResponse, _> = serde_json::from_str(&text);
        let mut out = parsed
            .map_err(|e| anyhow!("hyperliquid exchange decode: {e}; body: {}", truncate(&text, 200)))?;
        out.raw = text;
        Ok(out)
    }

    /// Convert the JSON wire order into the signed HlOrder. LIMIT only: the venue
    /// market variant ({"market":{}}) is a distinct msgpack shape we do not emit yet —
    /// refusing beats hashing a shape the pinned vectors do not cover.
    pub fn wire_to_hl_order(order: &OrderWire) -> Result<HlOrder> {
        if let Some(tif) = order.order_type.get("limit").and_then(|l| l.get("tif")).and_then(|v| v.as_str()) {
            Ok(HlOrder {
                a: order.a,
                b: order.b,
                p: order.p.clone(),
                s: order.s.clone(),
                r: order.r,
                t: HlOrderType::Limit { tif: tif.to_string() },
            })
        } else {
            bail!("only limit orders are signable in this kit version (got order_type: {})", order.order_type);
        }
    }
/// Sign and POST the ONE-TIME `approveBuilderFee` action. WHO SIGNS MATTERS: the
/// caller's signer MUST be the user's MAIN wallet key (an API/agent wallet is
/// sign-only by venue design — the venue rejects approval attempts from it).
/// One approval per builder, revocable any time from the user's account.
/// `fee_tenths_bp`: the max fee the user allows us per fill, 1..=100 (10 bp cap).
pub fn approve_builder_fee(&self, builder_addr: &str, fee_tenths_bp: u32, nonce: u64, is_mainnet: bool) -> Result<ExchangeResponse> {
    let mp = crate::signer::approve_builder_fee_action_msgpack(builder_addr, fee_tenths_bp)?;
    let (_, sig) = self.signer.sign_l1_action(&mp, nonce, None, is_mainnet)?;
    let b = builder_addr.trim().to_ascii_lowercase();
    let max_fee_rate = format!("{}%", fee_tenths_bp as f64 / 1000.0);
    let action = serde_json::json!({
        "type": "approveBuilderFee",
        "maxFeeRate": max_fee_rate,
        "builder": b,
    });
    let body = Self::envelope(action, nonce, &sig);
    self.post_body(body)
}

/// Keyless info query: has a user approved this builder, and up to what fee?
/// POSTs {"type":"maxBuilderFee","user":<user>,"builder":<builder>} to /info.
/// Returns the approved max fee in tenths of bp (0 = no active approval).
pub fn max_builder_fee(&self, user: &str, builder: &str) -> Result<u32> {
    let body = serde_json::json!({ "type": "maxBuilderFee", "user": user.trim().to_ascii_lowercase(), "builder": builder.trim().to_ascii_lowercase() });
    let url = format!("{}/info", self.base);
    let resp = self.http.post(&url).json(&body).send()?;
    let status = resp.status();
    let text = resp.text()?;
    if !status.is_success() {
        bail!("maxBuilderFee {} -> HTTP {}: {}", url, status, truncate(&text, 200));
    }
    serde_json::from_str::<Value>(&text)
        .ok()
        .and_then(|v| v.as_i64().map(|n| n.max(0) as u32))
        .ok_or_else(|| anyhow!("maxBuilderFee: unexpected body: {}", truncate(&text, 200)))
}
}


fn truncate(s: &str, n: usize) -> &str {
    match s.char_indices().nth(n) {
        Some((i, _)) => &s[..i],
        None => s,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::signer::{AgentSigner, HlOrder, HlOrderAction, HlOrderType};

    // Test-only key; never a real wallet.
    const TEST_KEY: &str = "0x4646464646464646464646464646464646464646464646464646464646464646";
    const BUILDER: &str = "0x1111111111111111111111111111111111111111";

    #[test]
    fn envelope_body_construction_with_builder_fee() {
        let signer = AgentSigner::from_hex_key(TEST_KEY).unwrap();
        let order = OrderWire::limit(1, true, "2000.0", "0.1", false, "Gtc");
        validate_builder_fee(20, &order).expect("2 bp on a perp buy is allowed");

        let action = HlOrderAction {
            orders: vec![HlOrder {
                a: 1,
                b: true,
                p: "2000.0".to_string(),
                s: "0.1".to_string(),
                r: false,
                t: HlOrderType::Limit { tif: "Gtc".to_string() },
            }],
            grouping: "na".to_string(),
            builder: Some(HlBuilder { b: BUILDER.to_string(), f: 20 }),
        };
        let nonce: u64 = 1_730_000_000_000;
        let mp = action.to_msgpack().unwrap();
        let (_, sig) = signer.sign_l1_action(&mp, nonce, None, false).unwrap();
        let body = ExchangeClient::envelope(action.to_json(), nonce, &sig);

        // top-level fields exactly: action, nonce, signature
        let keys: Vec<&str> = body.as_object().unwrap().keys().map(|k| k.as_str()).collect();
        assert_eq!(keys, vec!["action", "nonce", "signature"]);
        assert_eq!(body["nonce"], serde_json::json!(1_730_000_000_000u64));

        let act = &body["action"];
        assert_eq!(act["type"], "order");
        assert_eq!(act["grouping"], "na");
        assert_eq!(act["builder"]["b"], BUILDER);
        assert_eq!(act["builder"]["f"], 20);
        let o = &act["orders"][0];
        assert_eq!(o["a"], 1);
        assert_eq!(o["b"], true);
        assert_eq!(o["p"], "2000.0");
        assert_eq!(o["s"], "0.1");
        assert_eq!(o["t"]["limit"]["tif"], "Gtc");

        let s = &body["signature"];
        assert_eq!(s["r"].as_str().unwrap().len(), 66, "r: 0x + 64 hex");
        assert_eq!(s["s"].as_str().unwrap().len(), 66, "s: 0x + 64 hex");
        assert!(s["r"].as_str().unwrap().starts_with("0x"));
        assert!(s["v"].as_u64().unwrap() == 27 || s["v"].as_u64().unwrap() == 28);
    }

    #[test]
    fn wire_order_converts_to_signed_hl_order() {
        // JSON wire order -> HlOrder for signing: the limit variant maps 1:1.
        let order = OrderWire::limit(5, false, "30000.0", "0.25", true, "Ioc");
        let hl = ExchangeClient::wire_to_hl_order(&order).unwrap();
        assert_eq!(hl.a, 5);
        assert_eq!(hl.b, false);
        assert_eq!(hl.p, "30000.0");
        assert_eq!(hl.s, "0.25");
        assert_eq!(hl.r, true);
        assert_eq!(
            hl.t,
            HlOrderType::Limit { tif: "Ioc".to_string() }
        );
    }

    #[test]
    fn wire_order_rejects_market_and_spot_buy_with_fee() {
        let market = OrderWire::market(1, true, "0.1", false);
        assert!(
            ExchangeClient::wire_to_hl_order(&market).is_err(),
            "market order_type JSON is not convertible yet; kit signs LIMIT orders only"
        );
        let spot_buy = OrderWire::limit(10_000, true, "10.0", "1.0", false, "Gtc");
        assert!(
            validate_builder_fee(20, &spot_buy).is_err(),
            "builder codes do not apply to the buying side of spot"
        );
    }

    #[test]
    fn builder_fee_validation_rejects_cap() {
        let perp = OrderWire::limit(1, true, "2000.0", "0.1", false, "Gtc");
        assert!(validate_builder_fee(20, &perp).is_ok(), "f=20 (2 bp) fine");
        assert!(validate_builder_fee(100, &perp).is_ok(), "f=100 (10 bp) at cap");
        assert!(validate_builder_fee(101, &perp).is_err(), "f=101 over perp cap");
    }

    #[test]
    fn exchange_response_parses_ok_vs_err() {
        let ok: ExchangeResponse = serde_json::from_str(r#"{"status":"ok","response":{"type":"order"}}"#).unwrap();
        assert!(ok.is_ok());
        let err: ExchangeResponse = serde_json::from_str(r#"{"status":"err","response":"User or API Wallet 0x... does not exist"}"#).unwrap();
        assert!(!err.is_ok());
        assert_eq!(err.error_text(), "User or API Wallet 0x... does not exist");
    }
    #[test]
    fn approve_builder_fee_msgpack_shape_is_named_map_in_declaration_order() {
        // {"type":"approveBuilderFee","maxFeeRate":"0.02%","builder":"0x1111…"}
        let mp = crate::signer::approve_builder_fee_action_msgpack(BUILDER, 20).unwrap();
        // fixmap(3), "type", fixstr, "approveBuilderFee" is 17 chars -> str8, ...
        assert_eq!(mp[0], 0x83, "3-entry map");
        let s = String::from_utf8_lossy(&mp);
        assert!(s.contains("approveBuilderFee"), "action type key value present: {s}");
        assert!(s.contains("maxFeeRate"), "maxFeeRate field present");
        assert!(s.contains("0.02%"), "2 bp as decimal percent string, got {s}");
        assert!(s.to_ascii_lowercase().contains(BUILDER.to_ascii_lowercase().as_str()), "builder address present");
    }

    #[test]
    fn approve_builder_fee_rejects_bad_address_and_fee() {
        assert!(crate::signer::approve_builder_fee_action_msgpack("not-an-address", 20).is_err());
        assert!(crate::signer::approve_builder_fee_action_msgpack(BUILDER, 0).is_err());
        assert!(crate::signer::approve_builder_fee_action_msgpack(BUILDER, 101).is_err());
    }

    #[test]
    fn approve_builder_fee_end_to_end_signs_and_recovers() {
        // Full path with a throwaway key: msgpack -> l1 hash -> EIP-712 sign -> recover.
        let signer = AgentSigner::from_hex_key(TEST_KEY).unwrap();
        let mp = crate::signer::approve_builder_fee_action_msgpack(BUILDER, 50).unwrap();
        let nonce: u64 = 1_739_000_000_000;
        let (connection_id, sig) = signer.sign_l1_action(&mp, nonce, None, false).unwrap();
        let recovered = AgentSigner::recover_l1_action(connection_id, false, &sig).unwrap();
        assert_eq!(recovered, signer.address(), "signature verifies against the signing key");
    }
}
