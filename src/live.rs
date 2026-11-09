//! LIVE ORDER PATH (bet 2). One MCP tool that can put a REAL order on Hyperliquid:
//! the USER'S OWN sign-only agent key signs it, the guardrail gate runs BEFORE any
//! signature exists, and our builder code rides along only when the venue itself
//! reports that the user's main wallet approved us (`maxBuilderFee` >= our fee).
//!
//! Design laws (the signed-order section of this README, CEO bet bet2-hl-agent-first-user):
//! - The dry-run simulator stays the default. This module stays unconstructed
//!   unless the operator passes `--live` AND an env var name holding the agent key
//!   AND the user's own main-wallet address. No key is read from disk or .env.
//! - Guardrail first, sign second. `Venue::sign_and_post` is the only code path
//!   that can produce a signature, and it is called at most once per request; a
//!   rejected request returns a `LiveOutcome::Rejected { signed: false, sent: false }`
//!   and a test asserts the fake venue's sign counter stayed at zero.
//! - Builder code is omitted (never guessed) whenever the venue's approval is
//!   missing or smaller than our fee: the venue would reject the whole order
//!   otherwise, and a fee we did not earn is not ours to take.
//! - No retry. One HTTP attempt; a venue error is returned verbatim plus one
//!   sentence explaining the errors real users hit (min order value, unknown agent
//!   wallet, nonce).
use crate::exchange::{ExchangeClient, ExchangeResponse, OrderWire};
use crate::guardrail::{Ctx, Decision, Guardrail, OrderIntent};
use crate::hl::HlClient;
use crate::signer::{HlBuilder, HlOrderAction};
use anyhow::{bail, Result};
use serde_json::{json, Value};

/// What the caller asks for. `limit_price` is REQUIRED: this kit version signs
/// limit orders only (a market order is a different msgpack shape the pinned
/// vectors do not cover), so users send an IoC limit for an immediate fill-or-nothing.
#[derive(Debug, Clone)]
pub struct LiveOrderRequest {
    pub coin: String,
    pub is_buy: bool,
    pub size: f64,
    pub limit_price: f64,
    /// Gtc | Ioc | Alo
    pub tif: String,
    pub reduce_only: bool,
}

impl LiveOrderRequest {
    pub fn intent(&self) -> OrderIntent {
        OrderIntent {
            coin: self.coin.clone(),
            is_buy: self.is_buy,
            size: self.size,
            limit_price: Some(self.limit_price),
            reduce_only: self.reduce_only,
            intent_id: None,
        }
    }
}

/// Operator configuration for the live path. The key itself is NOT here: it lives
/// inside the AgentSigner that the venue holds.
#[derive(Debug, Clone)]
pub struct LiveConfig {
    /// The user's MAIN wallet address (the account this agent key trades for).
    pub user: String,
    /// Our builder address. None => orders go out with no builder field at all.
    pub builder: Option<String>,
    /// Max fee we ask for, in tenths of a basis point (20 = 2 bp). 1..=100.
    pub fee_tenths_bp: u32,
    pub is_mainnet: bool,
}

/// The venue seam. Production implements it with two read calls and one exchange
/// POST; tests inject a fake so the whole live path runs with no network and no
/// signature that could ever reach the real venue.
pub trait Venue {
    fn mark(&self, coin: &str) -> Result<Option<f64>>;
    /// Perp asset index from the venue's own universe (never a hard-coded map).
    fn asset_index(&self, coin: &str) -> Result<u32>;
    /// Size decimals the venue allows for that asset index.
    fn size_decimals(&self, coin: &str) -> Result<u32>;
    /// The venue's own view of the user's approved max builder fee (tenths of bp).
    fn max_builder_fee(&self, user: &str, builder: &str) -> Result<u32>;
    /// Current |position| notional (USD) on the user's account for `coin`.
    /// The position cap CANNOT be enforced without this: the guardrail engine is
    /// pure and sees no account. `Err` means "unknown" and the order is refused.
    fn position_notional(&self, _user: &str, _coin: &str, _mark: f64) -> Result<f64> {
        bail!("this venue cannot report account positions, so the position cap cannot be enforced")
    }
    /// Venue-reported realized PnL (USD) since `start_ms`, to seed the daily-loss cap.
    fn realized_pnl_since(&self, _user: &str, _start_ms: i64) -> Result<f64> {
        bail!("this venue cannot report realized PnL, so the daily-loss cap cannot be enforced")
    }
    /// THE ONLY signing route in the live path. Returns the signed body (for the
    /// user's audit trail) and the result of the POST. A failed POST still returns
    /// the signed body: the caller must be able to say "this may be live, check
    /// open orders" instead of a bare error that invites a double-filling retry.
    /// This is the REQUIRED method on purpose — an impl that could drop the body on
    /// a transport error would reintroduce that bug for every user of the kit.
    fn sign_and_post_audited(
        &self,
        action: &HlOrderAction,
        nonce: u64,
        is_mainnet: bool,
    ) -> Result<(Value, anyhow::Result<ExchangeResponse>)>;
    /// Convenience for callers that treat any POST failure as fatal. Built on the
    /// audited method so there is exactly one signing path.
    fn sign_and_post(
        &self,
        action: &HlOrderAction,
        nonce: u64,
        is_mainnet: bool,
    ) -> Result<(Value, ExchangeResponse)> {
        let (body, resp) = self.sign_and_post_audited(action, nonce, is_mainnet)?;
        let resp = resp?;
        Ok((body, resp))
    }
}

/// Production venue: public /info via HlClient + the signed /exchange client.
pub struct HlVenue {
    pub hl: HlClient,
    pub ex: ExchangeClient,
}

impl Venue for HlVenue {
    fn mark(&self, coin: &str) -> Result<Option<f64>> {
        // An unknown coin is not a transport failure: the gate still runs and the
        // venue answers for the asset index.
        Ok(self.hl.mid_of(coin).ok())
    }

    fn asset_index(&self, coin: &str) -> Result<u32> {
        let meta = self.hl.meta()?;
        meta.universe
            .iter()
            .position(|a| a.name.eq_ignore_ascii_case(coin))
            .map(|i| i as u32)
            .ok_or_else(|| anyhow::anyhow!("coin {coin:?} is not in the Hyperliquid perp universe"))
    }

    fn size_decimals(&self, coin: &str) -> Result<u32> {
        let meta = self.hl.meta()?;
        meta.universe
            .iter()
            .find(|a| a.name.eq_ignore_ascii_case(coin))
            .map(|a| a.sz_decimals)
            .ok_or_else(|| anyhow::anyhow!("coin {coin:?} is not in the Hyperliquid perp universe"))
    }

    fn max_builder_fee(&self, user: &str, builder: &str) -> Result<u32> {
        self.ex.max_builder_fee(user, builder)
    }

    fn position_notional(&self, user: &str, coin: &str, mark: f64) -> Result<f64> {
        self.hl.position_notional(user, coin, mark)
    }

    fn realized_pnl_since(&self, user: &str, start_ms: i64) -> Result<f64> {
        self.hl.realized_pnl_since(user, start_ms)
    }

    fn sign_and_post_audited(
        &self,
        action: &HlOrderAction,
        nonce: u64,
        is_mainnet: bool,
    ) -> Result<(Value, anyhow::Result<ExchangeResponse>)> {
        self.ex.sign_and_post_audited(action, nonce, is_mainnet)
    }
}

/// What happened. Every variant carries the numbers a user needs to check us.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum LiveOutcome {
    /// Guardrail (or a local sanity check) said no. NOTHING was signed, NOTHING sent.
    Rejected { reason: String, signed: bool, sent: bool },
    /// The venue accepted the action; `receipt` says what the order did.
    Placed {
        signed: bool,
        sent: bool,
        builder_attached: bool,
        builder_omitted_reason: Option<String>,
        nonce: u64,
        receipt: String,
        venue_response: Value,
        /// Verbatim venue body, for the user's own audit trail.
        raw: String,
    },
    /// The envelope said "ok" but the PER-ORDER status carries an error (minimum
    /// order value, bad price, insufficient margin...). Nothing rests on the book.
    /// Distinct from `Placed` on purpose: an agent host switching on `outcome` must
    /// never believe it holds a position the venue refused.
    VenueRejected {
        signed: bool,
        sent: bool,
        builder_attached: bool,
        nonce: u64,
        error: String,
        explanation: String,
        receipt: String,
        raw: String,
    },
    /// The action was SIGNED and the POST left this process, but no usable answer
    /// came back (timeout, 5xx, connection reset). The order MAY be live. Check open
    /// orders before doing anything else: a retry with a fresh nonce can double-fill.
    Unknown {
        signed: bool,
        sent: Option<bool>,
        nonce: u64,
        error: String,
        signed_body: Option<Value>,
        next_step: String,
    },
    /// The venue answered with status "err". No retry was made.
    VenueError {
        signed: bool,
        sent: bool,
        builder_attached: bool,
        nonce: u64,
        error: String,
        explanation: String,
        raw: String,
    },
}

pub struct LiveTrader<V: Venue> {
    venue: V,
    pub cfg: LiveConfig,
    pub gate: Guardrail,
    /// The signed body of the last order that reached the signer (audit trail for
    /// the user). None when nothing was signed.
    pub last_signed_body: Option<Value>,
}

impl<V: Venue> LiveTrader<V> {
    pub fn new(venue: V, cfg: LiveConfig, gate: Guardrail) -> Self {
        Self { venue, cfg, gate, last_signed_body: None }
    }

    pub fn place(&mut self, req: &LiveOrderRequest, now_ms: i64) -> Result<LiveOutcome> {
        // 0) Arguments a user can typo: name the argument, do not call the venue.
        if !matches!(req.tif.as_str(), "Gtc" | "Ioc" | "Alo") {
            return Ok(reject(format!("tif must be Gtc, Ioc or Alo, got {:?}", req.tif)));
        }
        let mark = self.venue.mark(&req.coin)?;
        // A missing mark is NOT a reason to skip the USD caps: the notional, leverage
        // and position checks all need a price, and the engine (correctly) skips them
        // when it has none. So an unknown mark refuses the order instead of signing
        // it with every USD cap switched off.
        let mark = match mark {
            Some(m) if m.is_finite() && m > 0.0 => m,
            _ => {
                return Ok(reject(format!(
                    "no usable mark price for {} — the USD caps (order, leverage, position) cannot be checked, so this order was NOT signed. Retry when the venue publishes a mid for it.",
                    req.coin
                )))
            }
        };

        // 1) GUARDRAILS, BEFORE ANY SIGNATURE EXISTS. The engine is pure, so it needs
        //    the account state handed to it: the real position notional (else the
        //    position cap never runs) and the venue's realized PnL today (else the
        //    daily-loss cap can never trip). Both reads fail CLOSED: an unknown
        //    account state refuses the order rather than signing it unguarded.
        let position_notional = match self.venue.position_notional(&self.cfg.user, &req.coin, mark) {
            Ok(p) if p.is_finite() && p >= 0.0 => p,
            Ok(p) => return Ok(reject(format!("venue reported a nonsense position notional ({p}) for {}", req.coin))),
            Err(e) => {
                return Ok(reject(format!(
                    "could not read your current {} position ({e}), so the position cap cannot be enforced — order NOT signed",
                    req.coin
                )))
            }
        };
        let day_start_ms = now_ms.div_euclid(86_400_000) * 86_400_000;
        match self.venue.realized_pnl_since(&self.cfg.user, day_start_ms) {
            Ok(pnl) if pnl.is_finite() => {
                // SET, never add: the venue's figure already covers the whole day.
                self.gate.set_realized_pnl_today(pnl, now_ms);
            }
            Ok(pnl) => return Ok(reject(format!("venue reported a nonsense realized PnL ({pnl}) for today"))),
            Err(e) => {
                return Ok(reject(format!(
                    "could not read today's realized PnL ({e}), so the daily-loss cap cannot be enforced — order NOT signed"
                )))
            }
        }
        let intent = req.intent();
        let decision = self.gate.allow(
            &intent,
            Ctx { now_ms, mark: Some(mark), position_notional: Some(position_notional) },
        );
        if let Decision::Reject { reason } = decision {
            return Ok(reject(format!("guardrail: {reason}")));
        }

        // 2) BUILDER APPROVAL: ask the venue, never assume. Without an approval at
        //    least as large as our fee the order goes out with NO builder field.
        let mut builder_omitted_reason = None;
        let mut builder = None;
        if let Some(addr) = self.cfg.builder.clone() {
            if self.cfg.fee_tenths_bp == 0 || self.cfg.fee_tenths_bp > 100 {
                bail!("builder fee {} is outside 1..=100 tenths of a bp", self.cfg.fee_tenths_bp);
            }
            match self.venue.max_builder_fee(&self.cfg.user, &addr) {
                Ok(approved) if approved >= self.cfg.fee_tenths_bp => {
                    builder = Some(HlBuilder { b: addr.to_ascii_lowercase(), f: self.cfg.fee_tenths_bp });
                }
                Ok(approved) => {
                    builder_omitted_reason = Some(format!(
                        "user approved {approved} tenths of a bp for {addr}, below our {} — order sent WITHOUT a builder fee; run examples/approve_builder_fee.rs to let our fee ride along",
                        self.cfg.fee_tenths_bp
                    ));
                }
                Err(e) => {
                    builder_omitted_reason = Some(format!(
                        "could not read the builder approval ({e}) — order sent WITHOUT a builder fee"
                    ));
                }
            }
        }

        // 3) Wire order + signed action. Limit only, size in the venue's decimals.
        let a = self.venue.asset_index(&req.coin)?;
        let dp = self.venue.size_decimals(&req.coin)?;
        let wire = OrderWire::limit(a, req.is_buy, &fmt_price(req.limit_price), &fmt_size(req.size, dp), req.reduce_only, &req.tif);
        let action = HlOrderAction {
            orders: vec![ExchangeClient::wire_to_hl_order(&wire)?],
            grouping: "na".to_string(),
            builder: builder.clone(),
        };

        // 4) Sign + POST exactly once.
        let nonce = now_ms.max(0) as u64;
        let (body, resp) = match self.venue.sign_and_post_audited(&action, nonce, self.cfg.is_mainnet) {
            Ok(pair) => pair,
            // Even a signing failure must not look like "nothing happened" if we
            // cannot rule out that bytes left: report unknown, keep the nonce.
            Err(e) => {
                return Ok(LiveOutcome::Unknown {
                    signed: false,
                    sent: None,
                    nonce,
                    error: e.to_string(),
                    signed_body: None,
                    next_step: "Nothing was signed (the signer refused), so no order can exist. Safe to retry after fixing the reason above.".into(),
                })
            }
        };
        let builder_attached = builder.is_some();
        // The POST itself failed: we hold a signature but no answer. NEVER a bare error,
        // never a retry — the user has to check the book first.
        let resp = match resp {
            Ok(r) => r,
            Err(e) => {
                let signed_body = Some(body.clone());
                self.last_signed_body = signed_body.clone();
                return Ok(LiveOutcome::Unknown {
                    signed: true,
                    sent: None,
                    nonce,
                    error: e.to_string(),
                    signed_body,
                    next_step: "The order was signed and the request was sent, but no answer came back. Do NOT retry: check your open orders and fills on Hyperliquid first, because this order may be live.".into(),
                });
            }
        };
        self.last_signed_body = Some(body);
        if !resp.is_ok() {
            let error = resp.error_text();
            return Ok(LiveOutcome::VenueError {
                signed: true,
                sent: true,
                builder_attached,
                nonce,
                explanation: explain(&error),
                error,
                raw: resp.raw,
            });
        }
        let receipt = receipt_from_order_response(&resp.response);
        // The envelope's "ok" is not per-order success. Hyperliquid answers HTTP 200 /
        // status ok with {"data":{"statuses":[{"error":"..."}]}} for a refused order.
        // Reporting that as `placed` would tell an agent it holds a position it does
        // not have, so the per-order error wins.
        if let Some(err) = per_order_error(&resp.response) {
            return Ok(LiveOutcome::VenueRejected {
                signed: true,
                sent: true,
                builder_attached,
                nonce,
                explanation: explain(&err),
                error: err,
                receipt,
                raw: resp.raw,
            });
        }
        Ok(LiveOutcome::Placed {
            signed: true,
            sent: true,
            builder_attached,
            builder_omitted_reason,
            nonce,
            receipt,
            venue_response: resp.response,
            raw: resp.raw,
        })
    }

    /// The JSON the MCP tool returns. Pure formatting over the outcome.
    pub fn outcome_json(outcome: &LiveOutcome, signed_body: Option<&Value>) -> Value {
        let mut v = serde_json::to_value(outcome).unwrap_or_else(|_| json!({ "outcome": "unknown" }));
        if let Some(body) = signed_body {
            if let Value::Object(m) = &mut v {
                m.insert("signed_body".into(), body.clone());
            }
        }
        v
    }
}

fn reject(reason: String) -> LiveOutcome {
    LiveOutcome::Rejected { reason, signed: false, sent: false }
}

/// Price as the venue wants it: plain decimal, at most 6 decimals, no exponent
/// (binary floats never reach the wire).
pub fn fmt_price(p: f64) -> String {
    let s = format!("{p:.6}");
    let s = s.trim_end_matches('0').trim_end_matches('.').to_string();
    if s.is_empty() { "0".into() } else { s }
}

/// Size rounded DOWN to the venue's size decimals (asking for less is safe; asking
/// for a size the venue cannot represent is a rejected order).
pub fn fmt_size(sz: f64, decimals: u32) -> String {
    let f = 10f64.powi(decimals.min(8) as i32);
    let v = (sz * f).floor() / f;
    let s = format!("{v:.prec$}", prec = decimals.min(8) as usize);
    if s.contains('.') { s.trim_end_matches('0').trim_end_matches('.').to_string() } else { s }
}

/// Venue error -> one sentence the user can act on. These are the strings the real
/// API sends (see README "Errors you will actually hit").
pub fn explain(error: &str) -> String {
    let e = error.to_ascii_lowercase();
    if e.contains("does not exist") && (e.contains("wallet") || e.contains("user")) {
        return "The agent wallet is not registered for that account: create an API wallet in the Hyperliquid UI while signed in as the main wallet, and use its private key together with that main wallet's address as --user.".into();
    }
    if e.contains("minimum") || e.contains("min order") || e.contains("too small") || e.contains("$10") {
        return "Below the venue minimum (about $10 notional per order): raise size or price until the notional clears it.".into();
    }
    if e.contains("nonce") {
        return "Hyperliquid rejected the nonce (it must sit inside the venue's window and never repeat): wait a moment, run one agent key per process, and never replay a signed action.".into();
    }
    if e.contains("insufficient") || e.contains("margin") {
        return "Not enough margin for this size: lower the size or deposit first — the agent key can only trade what the account holds.".into();
    }
    if e.contains("reduce") {
        return "This is a reduce-only order but the account holds no position on that side to reduce.".into();
    }
    if e.contains("immediately match") || e.contains("ioc") {
        return "An IoC order did not fill and the venue cancelled it (normal when the book moved): resend at the new price if you still want the trade.".into();
    }
    format!("The venue refused the order: {error}")
}

/// Human receipt from the venue's order response. PURE: no clock, no network.
/// Walks both shapes the API uses: {"data":{"statuses":[ … ]}} and a bare {"statuses":[ … ]}.
/// The FIRST per-order error in a venue answer, if any. Pure.
pub fn per_order_error(response: &Value) -> Option<String> {
    let status = response
        .get("data")
        .and_then(|d| d.get("statuses"))
        .or_else(|| response.get("statuses"))
        .and_then(|s| s.as_array())?;
    status
        .iter()
        .find_map(|s| s.get("error").and_then(|e| e.as_str()).map(|e| e.to_string()))
}

pub fn receipt_from_order_response(response: &Value) -> String {

    let status = response
        .get("data")
        .and_then(|d| d.get("statuses"))
        .or_else(|| response.get("statuses"))
        .and_then(|s| s.as_array())
        .and_then(|a| a.first());
    match status {
        Some(s) => receipt_from(s),
        None => format!("UNKNOWN — no per-order status in the venue response: {response}. Do not assume a fill; check your open orders."),
    }
}

/// Receipt for ONE entry of `statuses`. PURE.
pub fn receipt_from(status: &Value) -> String {
    if let Some(rest) = status.get("resting") {
        let oid = rest.get("oid").and_then(|v| v.as_i64()).unwrap_or(0);
        return format!("RESTING — order {oid} is live on the book and has NOT filled yet. Check the oid in the Hyperliquid UI before assuming a position.");
    }
    if let Some(fill) = status.get("filled") {
        let sz = fill.get("totalSz").and_then(|v| v.as_str()).unwrap_or("?");
        let px = fill.get("avgPx").and_then(|v| v.as_str()).unwrap_or("?");
        let oid = fill.get("oid").and_then(|v| v.as_i64()).unwrap_or(0);
        return format!("FILLED — {sz} at an average of {px} (oid {oid}). This is the venue's own fill record for your order.");
    }
    if let Some(err) = status.get("error").and_then(|v| v.as_str()) {
        return format!("NOT FILLED — {}", explain(err));
    }
    format!("UNKNOWN — the venue sent a status this kit does not recognise: {status}. Do not assume a fill; check your open orders.")
}

/// Parse the MCP tool arguments into a request, with messages that name the field.
impl LiveOrderRequest {
    pub fn from_args(args: &Value) -> Result<Self, String> {
        let coin = args.get("coin").and_then(|v| v.as_str()).ok_or("missing coin")?.to_string();
        let side = args.get("side").and_then(|v| v.as_str()).ok_or("missing side")?;
        let is_buy = match side {
            "buy" => true,
            "sell" => false,
            other => return Err(format!("side must be \"buy\" or \"sell\", got {other:?}")),
        };
        let size = args.get("size").and_then(|v| v.as_f64()).ok_or("missing size (base-asset units)")?;
        let limit_price = args
            .get("limit_price")
            .and_then(|v| v.as_f64())
            .ok_or("missing limit_price: this kit signs LIMIT orders only — send an IoC limit at the price you accept")?;
        let tif = args.get("tif").and_then(|v| v.as_str()).unwrap_or("Ioc").to_string();
        let reduce_only = args.get("reduce_only").and_then(|v| v.as_bool()).unwrap_or(false);
        Ok(Self { coin, is_buy, size, limit_price, tif, reduce_only })
    }
}

/// What the MCP server needs from the live path. A trait object so the server tests
/// can drive a fake venue through the real JSON-RPC boundary.
pub trait LiveTool {
    fn place_json(&mut self, args: &Value, now_ms: i64) -> Result<Value, String>;
    /// True when a builder address is configured (the tool description says so).
    fn builder_configured(&self) -> bool;
}

impl<V: Venue> LiveTool for LiveTrader<V> {
    fn place_json(&mut self, args: &Value, now_ms: i64) -> Result<Value, String> {
        let req = LiveOrderRequest::from_args(args)?;
        let outcome = self.place(&req, now_ms).map_err(|e| e.to_string())?;
        let mut v = serde_json::to_value(&outcome).map_err(|e| e.to_string())?;
        if let Some(body) = self.last_signed_body.take() {
            if let Value::Object(m) = &mut v {
                m.insert("signed_body".into(), body);
            }
        }
        Ok(v)
    }

    fn builder_configured(&self) -> bool {
        self.cfg.builder.is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::guardrail::GuardrailConfig;
    use std::cell::RefCell;

    const USER: &str = "0x2222222222222222222222222222222222222222";
    const BUILDER: &str = "0x1111111111111111111111111111111111111111";

    /// Fake venue: records every signing request; no network, no key.
    struct FakeVenue {
        mark: Option<f64>,
        approved: u32,
        /// |position| notional the fake account reports for the coin.
        position: RefCell<f64>,
        /// realized PnL today the fake account reports.
        pnl_today: RefCell<f64>,
        sign_calls: RefCell<u32>,
        posts: RefCell<Vec<Value>>,
        answer: RefCell<LiveOutcomeAnswer>,
    }
    enum LiveOutcomeAnswer {
        Ok(Value),
        Err(String),
        /// The POST left the process and no answer came back.
        Transport(String),
    }

    impl FakeVenue {
        fn new(mark: Option<f64>, approved: u32) -> Self {
            Self {
                mark,
                approved,
                position: RefCell::new(0.0),
                pnl_today: RefCell::new(0.0),
                sign_calls: RefCell::new(0),
                posts: RefCell::new(Vec::new()),
                answer: RefCell::new(LiveOutcomeAnswer::Ok(json!({"data": {"statuses": [{"filled": {"totalSz": "0.01", "avgPx": "2000.0", "oid": 77}}]}}))),
            }
        }
    }

    impl Venue for FakeVenue {
        fn mark(&self, _coin: &str) -> Result<Option<f64>> {
            Ok(self.mark)
        }
        fn asset_index(&self, _coin: &str) -> Result<u32> {
            Ok(1)
        }
        fn size_decimals(&self, _coin: &str) -> Result<u32> {
            Ok(2)
        }
        fn max_builder_fee(&self, _user: &str, _builder: &str) -> Result<u32> {
            Ok(self.approved)
        }
        fn position_notional(&self, _user: &str, _coin: &str, _mark: f64) -> Result<f64> {
            Ok(*self.position.borrow())
        }
        fn realized_pnl_since(&self, _user: &str, _start_ms: i64) -> Result<f64> {
            Ok(*self.pnl_today.borrow())
        }
        fn sign_and_post_audited(
            &self,
            action: &HlOrderAction,
            nonce: u64,
            _is_mainnet: bool,
        ) -> Result<(Value, anyhow::Result<ExchangeResponse>)> {
            *self.sign_calls.borrow_mut() += 1;
            let body = json!({"action": action.to_json(), "nonce": nonce, "signature": {"r": "0x1", "s": "0x2", "v": 27}});
            self.posts.borrow_mut().push(body.clone());
            let posted = match &*self.answer.borrow() {
                LiveOutcomeAnswer::Ok(v) => Ok(ExchangeResponse { status: "ok".into(), response: v.clone(), raw: v.to_string() }),
                LiveOutcomeAnswer::Err(e) => Ok(ExchangeResponse { status: "err".into(), response: json!(e), raw: json!({"status": "err", "response": e}).to_string() }),
                LiveOutcomeAnswer::Transport(e) => Err(anyhow::anyhow!("{e}")),
            };
            Ok((body, posted))
        }
    }

    fn cfg(approved_builder: bool) -> LiveConfig {
        LiveConfig {
            user: USER.into(),
            builder: if approved_builder { Some(BUILDER.into()) } else { None },
            fee_tenths_bp: 20,
            is_mainnet: false,
        }
    }

    fn gate() -> Guardrail {
        Guardrail::new(GuardrailConfig {
            max_position_notional_usd: 1000.0,
            max_order_notional_usd: 50.0, // tiny, so an oversized test order trips it
            max_leverage: 1.0,
            account_equity_usd: 500.0,
            daily_loss_cap_usd: 20.0,
            max_similar_actions: 3,
            loop_window_seconds: 60,
            max_calls_per_hour: 10,
        })
    }

    fn req(size: f64) -> LiveOrderRequest {
        LiveOrderRequest { coin: "ETH".into(), is_buy: true, size, limit_price: 2000.0, tif: "Ioc".into(), reduce_only: false }
    }

    #[test]
    fn guardrail_reject_never_signs_and_never_sends() {
        let venue = FakeVenue::new(Some(2000.0), 20);
        let mut t = LiveTrader::new(venue, cfg(true), gate());
        // 1 ETH at 2000 = $2000 notional, far above the $50 cap.
        let out = t.place(&req(1.0), 1_730_000_000_000).unwrap();
        match out {
            LiveOutcome::Rejected { reason, signed, sent } => {
                assert!(reason.contains("guardrail"), "reason names the gate: {reason}");
                assert!(!signed && !sent);
            }
            other => panic!("expected Rejected, got {other:?}"),
        }
        assert_eq!(*t.venue.sign_calls.borrow(), 0, "a guardrail rejection must not reach the signer");
        assert!(t.venue.posts.borrow().is_empty(), "nothing may be POSTed");
    }

    #[test]
    fn no_approval_means_no_builder_field_but_the_order_still_goes() {
        let venue = FakeVenue::new(Some(2000.0), 0); // venue says: not approved
        let mut t = LiveTrader::new(venue, cfg(true), gate());
        let out = t.place(&req(0.01), 1_730_000_000_000).unwrap();
        match out {
            LiveOutcome::Placed { builder_attached, builder_omitted_reason, receipt, .. } => {
                assert!(!builder_attached, "no approval => no builder code");
                assert!(builder_omitted_reason.unwrap().contains("WITHOUT a builder fee"));
                assert!(receipt.starts_with("FILLED"), "receipt quotes the venue fill: {receipt}");
            }
            other => panic!("expected Placed, got {other:?}"),
        }
        let posts = t.venue.posts.borrow();
        assert_eq!(posts.len(), 1);
        assert!(posts[0]["action"].get("builder").is_none(), "builder key must be absent, got {}", posts[0]["action"]);
        assert_eq!(*t.venue.sign_calls.borrow(), 1);
    }

    #[test]
    fn approval_at_or_above_our_fee_attaches_the_builder_code() {
        let venue = FakeVenue::new(Some(2000.0), 25); // approved 2.5 bp, we ask 2 bp
        let mut t = LiveTrader::new(venue, cfg(true), gate());
        let out = t.place(&req(0.01), 1_730_000_000_000).unwrap();
        match out {
            LiveOutcome::Placed { builder_attached, .. } => assert!(builder_attached),
            other => panic!("expected Placed, got {other:?}"),
        }
        let posts = t.venue.posts.borrow();
        assert_eq!(posts[0]["action"]["builder"]["b"], BUILDER);
        assert_eq!(posts[0]["action"]["builder"]["f"], 20);
    }

    #[test]
    fn a_fee_above_the_approval_is_not_taken() {
        let venue = FakeVenue::new(Some(2000.0), 5); // user allowed 0.5 bp, we ask 2 bp
        let mut t = LiveTrader::new(venue, cfg(true), gate());
        let _ = t.place(&req(0.01), 1_730_000_000_000).unwrap();
        assert!(t.venue.posts.borrow()[0]["action"].get("builder").is_none(), "never charge above the user's approval");
    }

    #[test]
    fn no_builder_configured_means_no_builder_field() {
        let venue = FakeVenue::new(Some(2000.0), 100);
        let mut t = LiveTrader::new(venue, cfg(false), gate());
        let _ = t.place(&req(0.01), 1_730_000_000_000).unwrap();
        assert!(t.venue.posts.borrow()[0]["action"].get("builder").is_none());
    }

    #[test]
    fn venue_err_becomes_an_explained_error_not_a_retry() {
        let venue = FakeVenue::new(Some(2000.0), 20);
        *venue.answer.borrow_mut() = LiveOutcomeAnswer::Err("Order has invalid price or size".into());
        let mut t = LiveTrader::new(venue, cfg(true), gate());
        match t.place(&req(0.01), 1_730_000_000_000).unwrap() {
            LiveOutcome::VenueError { explanation, sent, .. } => {
                assert!(sent);
                assert!(explanation.contains("venue refused"), "got {explanation}");
            }
            other => panic!("expected VenueError, got {other:?}"),
        }
        assert_eq!(*t.venue.sign_calls.borrow(), 1, "exactly one attempt, no retry");
    }

    /// Critic B1: the position cap must use the account's REAL position. $900 already
    /// open + a $200 order against a $1000 cap must be refused before any signature.
    #[test]
    fn position_cap_uses_the_venue_reported_position() {
        let venue = FakeVenue::new(Some(2000.0), 20);
        *venue.position.borrow_mut() = 900.0;
        let mut t = LiveTrader::new(venue, cfg(true), gate());
        // 0.1 ETH at 2000 = $200 -> 900 + 200 = 1100 > $1000 position cap (order cap is $50,
        // so shrink the order cap check out of the way by using a small size at a high mark).
        let mut r = req(0.1);
        r.limit_price = 2000.0;
        t.gate.cfg.max_order_notional_usd = 10_000.0;
        let out = t.place(&r, 1_730_000_000_000).unwrap();
        match out {
            LiveOutcome::Rejected { reason, signed, sent } => {
                assert!(reason.contains("position cap"), "got {reason}");
                assert!(!signed && !sent);
            }
            other => panic!("expected Rejected, got {other:?}"),
        }
        assert_eq!(*t.venue.sign_calls.borrow(), 0, "position cap must refuse before signing");
    }

    /// Critic B2: the daily-loss cap must trip from the VENUE's PnL, not a counter that
    /// only the simulator ever feeds.
    #[test]
    fn daily_loss_cap_trips_from_the_venue_reported_pnl() {
        let venue = FakeVenue::new(Some(2000.0), 20);
        *venue.pnl_today.borrow_mut() = -25.0; // cap is -20
        let mut t = LiveTrader::new(venue, cfg(true), gate());
        let out = t.place(&req(0.01), 1_730_000_000_000).unwrap();
        match out {
            LiveOutcome::Rejected { reason, .. } => assert!(reason.contains("daily loss cap"), "got {reason}"),
            other => panic!("expected Rejected, got {other:?}"),
        }
        assert_eq!(*t.venue.sign_calls.borrow(), 0);
    }

    /// Seeding the daily-loss cap must REPLACE the venue figure, not add to it: two
    /// orders in one session would otherwise double-count the same loss and block an
    /// account that is still inside its cap.
    #[test]
    fn seeding_the_daily_loss_cap_twice_does_not_double_count() {
        let venue = FakeVenue::new(Some(2000.0), 20);
        *venue.pnl_today.borrow_mut() = -5.0; // 5 of a 20 cap used
        let mut t = LiveTrader::new(venue, cfg(true), gate());
        let _ = t.place(&req(0.01), 1_730_000_000_000).unwrap();
        let _ = t.place(&req(0.01), 1_730_000_000_001).unwrap();
        assert!(
            (t.gate.realized_pnl_today() - -5.0).abs() < 1e-9,
            "expected exactly the venue's -5.0, got {}",
            t.gate.realized_pnl_today()
        );
    }

    /// Critic B3: no mark => the USD caps cannot run => refuse. Silence is not consent.
    #[test]
    fn unknown_mark_refuses_instead_of_disabling_every_usd_cap() {
        let venue = FakeVenue::new(None, 20); // venue publishes no mid for the coin
        let mut t = LiveTrader::new(venue, cfg(true), gate());
        let out = t.place(&req(0.01), 1_730_000_000_000).unwrap();
        match out {
            LiveOutcome::Rejected { reason, signed, sent } => {
                assert!(reason.contains("mark"), "got {reason}");
                assert!(!signed && !sent);
            }
            other => panic!("expected Rejected, got {other:?}"),
        }
        assert_eq!(*t.venue.sign_calls.borrow(), 0);
    }

    /// Critic B3b: an unreadable account state must refuse, never assume flat/zero.
    #[test]
    fn unreadable_account_state_refuses_to_sign() {
        let venue = FakeVenue::new(Some(2000.0), 20);
        *venue.position.borrow_mut() = f64::NAN;
        let mut t = LiveTrader::new(venue, cfg(true), gate());
        let out = t.place(&req(0.01), 1_730_000_000_000).unwrap();
        assert!(matches!(out, LiveOutcome::Rejected { signed: false, sent: false, .. }), "got {out:?}");
    }

    /// Critic B4: a per-order venue error inside an "ok" envelope is NOT a placement.
    #[test]
    fn per_order_venue_error_is_reported_as_rejected_not_placed() {
        let venue = FakeVenue::new(Some(2000.0), 20);
        *venue.answer.borrow_mut() = LiveOutcomeAnswer::Ok(json!({
            "data": {"statuses": [{"error": "Order must have minimum value of $10."}]}
        }));
        let mut t = LiveTrader::new(venue, cfg(true), gate());
        match t.place(&req(0.01), 1_730_000_000_000).unwrap() {
            LiveOutcome::VenueRejected { error, explanation, signed, sent, .. } => {
                assert!(error.contains("minimum value"));
                assert!(explanation.contains("$10"), "explanation names the fix: {explanation}");
                assert!(signed && sent, "it WAS signed and sent; only the order was refused");
            }
            other => panic!("an ok envelope with a per-order error must not read as Placed, got {other:?}"),
        }
    }

    /// Critic B5: an ambiguous POST (signed, sent, no answer) must say so and tell the
    /// user to check open orders — never a bare error that invites a double-filling retry.
    #[test]
    fn ambiguous_post_reports_unknown_with_the_signed_body() {
        let venue = FakeVenue::new(Some(2000.0), 20);
        *venue.answer.borrow_mut() = LiveOutcomeAnswer::Transport("connection reset".into());
        let mut t = LiveTrader::new(venue, cfg(true), gate());
        match t.place(&req(0.01), 1_730_000_000_000).unwrap() {
            LiveOutcome::Unknown { signed, nonce, next_step, signed_body, .. } => {
                assert!(signed, "it was signed");
                assert!(nonce > 0);
                assert!(signed_body.is_some(), "the signed body must survive for the audit trail");
                assert!(next_step.contains("open orders"), "got {next_step}");
            }
            other => panic!("expected Unknown, got {other:?}"),
        }
        assert_eq!(*t.venue.sign_calls.borrow(), 1, "exactly one attempt, never a retry");
    }

    #[test]
    fn bad_tif_is_rejected_before_the_venue() {

        let venue = FakeVenue::new(Some(2000.0), 20);
        let mut t = LiveTrader::new(venue, cfg(true), gate());
        let mut r = req(0.01);
        r.tif = "Fok".into();
        assert!(matches!(t.place(&r, 1).unwrap(), LiveOutcome::Rejected { .. }));
        assert_eq!(*t.venue.sign_calls.borrow(), 0);
    }

    #[test]
    fn receipts_cover_the_three_real_shapes() {
        let filled = json!({"filled": {"totalSz": "0.01", "avgPx": "2000.0", "oid": 5}});
        assert!(receipt_from(&filled).starts_with("FILLED"));
        let resting = json!({"resting": {"oid": 9}});
        assert!(receipt_from(&resting).starts_with("RESTING"));
        let err = json!({"error": "Order must have minimum value of $10"});
        let r = receipt_from(&err);
        assert!(r.starts_with("NOT FILLED") && r.contains("minimum"), "{r}");
        // both response shapes resolve to the same receipt
        let wrapped = json!({"data": {"statuses": [filled]}});
        assert!(receipt_from_order_response(&wrapped).starts_with("FILLED"));
    }

    #[test]
    fn explanations_name_the_action_for_real_api_errors() {
        assert!(explain("User or API Wallet 0xabc does not exist").contains("API wallet"));
        assert!(explain("Order must have minimum value of $10").contains("$10"));
        assert!(explain("Nonce too low").contains("nonce"));
    }

    #[test]
    fn number_formatting_never_emits_exponents() {
        assert_eq!(fmt_price(2000.0), "2000");
        assert_eq!(fmt_price(0.000012345), "0.000012");
        assert_eq!(fmt_size(0.0199, 2), "0.01"); // floor, never round up past a cap
        assert_eq!(fmt_size(1.0, 4), "1");
        assert!(!fmt_price(1e-9).contains('e'));
    }
}
