//! The pre-trade gate: `allow(intent, ctx) -> Decision`, called BEFORE any signing.
use serde::{Deserialize, Serialize};

use crate::config::GuardrailConfig;

/// One order the agent wants to place.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OrderIntent {
    pub coin: String,
    pub is_buy: bool,
    /// Base-asset size (coin units), same convention as the exchange.
    pub size: f64,
    /// None = market (the caller fills at mark); Some = limit price.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit_price: Option<f64>,
    #[serde(default)]
    pub reduce_only: bool,
    /// Caller-supplied correlation id, echoed back untouched.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub intent_id: Option<String>,
}

/// Everything the gate needs that does not come from config or intent.
#[derive(Debug, Clone, Copy)]
pub struct Ctx {
    /// Wall clock in ms since epoch (UTC). Injected, never read from the system here.
    pub now_ms: i64,
    /// Current mark price for `intent.coin`, if known (None => no notional/leverage
    /// checks that need a price; the order is still tripwire/rate-limit/loss checked).
    pub mark: Option<f64>,
    /// Current |position| notional for `intent.coin`, if known (None => the caller
    /// applies the position cap itself, as product's simulator does pre-swap).
    pub position_notional: Option<f64>,
}

impl Ctx {
    /// Product-stub-compatible 2-field context (position cap enforced by caller).
    pub fn simple(now_ms: i64, mark: Option<f64>) -> Self {
        Self { now_ms, mark, position_notional: None }
    }
}

#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum Decision {
    Allow,
    Reject { reason: String },
}

impl Decision {
    pub fn is_allow(&self) -> bool {
        matches!(self, Decision::Allow)
    }

    /// Rejection reason, empty string when allowed.
    pub fn rejected_reason(&self) -> &str {
        match self {
            Decision::Allow => "",
            Decision::Reject { reason } => reason,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct Sig {
    coin: String,
    side: u8, // 1 buy, 0 sell
    /// Price bucket: 3 significant digits (~0.1% steps) — a retry chasing the book by a tick is still "near-identical".
    price_bucket: u64,
    /// Size bucket: 2 significant digits (~7% steps).
    size_bucket: u64,
}

/// Round `v` to `digits` significant digits as an integer bucket (None for
/// non-finite / non-positive input — such orders are rejected before this runs).
pub fn bucket(v: f64, digits: u32) -> Option<u64> {
    if !(v.is_finite()) || v <= 0.0 {
        return None;
    }
    let mag = v.abs().log10().floor() as i32;
    let scale = 10f64.powi(digits as i32 - 1 - mag);
    Some((v * scale).round() as u64)
}

/// Stateful gate. Sliding-window bookkeeping is internal; Ctx carries the clock.
#[derive(Debug)]
pub struct Guardrail {
    pub cfg: GuardrailConfig,
    window: Vec<(i64, Sig)>,
    call_times: Vec<i64>,
    day: i64,
    realized_pnl_today_usd: f64,
}

impl Guardrail {
    pub fn new(cfg: GuardrailConfig) -> Self {
        Self { cfg, window: Vec::new(), call_times: Vec::new(), day: 0, realized_pnl_today_usd: 0.0 }
    }

    /// UTC day rollover (epoch days). Callers may call freely; cheap.
    pub fn on_clock(&mut self, now_ms: i64) {
        let day = now_ms.div_euclid(86_400_000);
        if day != self.day {
            self.day = day;
            self.realized_pnl_today_usd = 0.0;
        }
    }

    /// Feed a realized PnL (USD) from a fill; negative values move toward the daily cap.
    /// Non-finite PnL is ignored (it must never advance or block the cap).
    pub fn record_realized_pnl(&mut self, pnl_usd: f64, now_ms: i64) {
        self.on_clock(now_ms);
        if pnl_usd.is_finite() {
            self.realized_pnl_today_usd += pnl_usd;
        }
    }

    pub fn realized_pnl_today(&self) -> f64 {
        self.realized_pnl_today_usd
    }

    /// The pre-trade gate. Rejection order: intent sanity -> daily loss -> rate limit ->
    /// loop tripwire -> price caps. An ALLOWED intent is recorded in both windows.
    pub fn allow(&mut self, intent: &OrderIntent, ctx: Ctx) -> Decision {
        self.on_clock(ctx.now_ms);

        // 0) sanity of the intent itself
        if intent.coin.is_empty() {
            return Decision::Reject { reason: "empty coin".into() };
        }
        if !intent.size.is_finite() || intent.size <= 0.0 {
            return Decision::Reject { reason: format!("size must be finite and > 0, got {}", intent.size) };
        }
        if let Some(px) = intent.limit_price {
            if !px.is_finite() || px <= 0.0 {
                return Decision::Reject { reason: format!("limit_price must be finite and > 0, got {px}") };
            }
        }

        // 0b) position cap when the caller knows the current position
        if let Some(pos) = ctx.position_notional {
            if !(pos.is_finite() && pos >= 0.0) {
                return Decision::Reject { reason: format!("position_notional must be finite and >= 0, got {pos}") };
            }
            let mark = match ctx.mark {
                Some(m) if m.is_finite() && m > 0.0 => m,
                _ => {
                    return Decision::Reject {
                        reason: "position cap check needs a finite positive mark when position_notional is supplied".into(),
                    }
                }
            };
            let is_reduce = intent.reduce_only || (pos > 0.0 && !intent.is_buy);
            if !is_reduce {
                let worst = if intent.is_buy { pos + intent.size * mark } else { (pos - intent.size * mark).abs() };
                if worst > self.cfg.max_position_notional_usd + 1e-9 {
                    return Decision::Reject {
                        reason: format!(
                            "position cap: |position after| {worst:.2} USD > max_position_notional {:.2}",
                            self.cfg.max_position_notional_usd
                        ),
                    };
                }
            }
        }

        // 1) daily loss cap
        if self.realized_pnl_today_usd <= -self.cfg.daily_loss_cap_usd {
            return Decision::Reject {
                reason: format!(
                    "daily loss cap hit: realized today {:.2} USD <= -{:.2}",
                    self.realized_pnl_today_usd, self.cfg.daily_loss_cap_usd
                ),
            };
        }

        // 2) rate limit (rolling hour)
        self.call_times.retain(|t| *t > ctx.now_ms - 3_600_000);
        if self.call_times.len() >= self.cfg.max_calls_per_hour {
            return Decision::Reject {
                reason: format!(
                    "rate limit: {} order intents in the last hour (cap {})",
                    self.call_times.len(),
                    self.cfg.max_calls_per_hour
                ),
            };
        }

        // 3) loop tripwire (sliding window of near-identical actions)
        let price_ref = intent.limit_price.or(ctx.mark);
        let sig = Sig {
            coin: intent.coin.clone(),
            side: u8::from(intent.is_buy),
            price_bucket: price_ref.and_then(|p| bucket(p, 3)).unwrap_or(0),
            size_bucket: bucket(intent.size, 2).unwrap_or(0),
        };
        self.window.retain(|(t, _)| *t > ctx.now_ms - self.cfg.loop_window_seconds * 1000);
        let similar = self.window.iter().filter(|(_, s)| *s == sig).count();
        if similar >= self.cfg.max_similar_actions {
            let side = if intent.is_buy { "buy" } else { "sell" };
            return Decision::Reject {
                reason: format!(
                    "loop tripwire: {similar} near-identical {side} {coin} intents within {}s (cap {})",
                    self.cfg.loop_window_seconds,
                    self.cfg.max_similar_actions,
                    coin = intent.coin,
                ),
            };
        }

        // 4) price-dependent caps (need a mark)
        if let Some(mark) = ctx.mark {
            if !(mark.is_finite() && mark > 0.0) {
                return Decision::Reject { reason: format!("mark must be finite and > 0, got {mark}") };
            }
            let notional = intent.size * mark;
            if notional > self.cfg.max_order_notional_usd + 1e-9 {
                return Decision::Reject {
                    reason: format!("order notional {notional:.2} USD > cap {:.2}", self.cfg.max_order_notional_usd),
                };
            }
            let max_notional = self.cfg.max_leverage * self.cfg.account_equity_usd;
            if notional > max_notional + 1e-9 {
                return Decision::Reject {
                    reason: format!(
                        "order notional {notional:.2} USD > max_leverage {} x equity {:.2} = {max_notional:.2}",
                        self.cfg.max_leverage, self.cfg.account_equity_usd
                    ),
                };
            }
        }

        // record the accepted action
        self.call_times.push(ctx.now_ms);
        self.window.push((ctx.now_ms, sig));
        Decision::Allow
    }
}
