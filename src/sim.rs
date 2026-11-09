//! Dry-run order simulator: intent -> guardrail gate -> simulated fill.
//! NO network order placement, NO keys, NO signing — the simulator only mutates its own
//! in-memory book. The fill-check step sits behind a trait so tech's real engine
//! (crates/guardrail, specs/hl-agent.md Week 1 tech) can drop in without touching callers.
use crate::guardrail::{Ctx, Decision, Guardrail, OrderIntent};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Fill outcome. Week 1: the stub always reports FILLED for accepted orders; the real
/// engine's fill-check (FILLED/PARTIAL/FAILED/UNKNOWN, deterministic) replaces it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum FillStatus {
    Filled,
    Partial,
    Failed,
    Unknown,
}

/// The fill-check seam. `check` MUST be deterministic given the fill.
pub trait FillCheck: Send {
    fn check(&self, fill: &SimFill) -> FillStatus;
}

/// Week-1 default: every gated order simulates as FILLED at the mark (or the limit).
/// Kept as the DryRunSimulator default in week 2 (the simulator path stays the default;
/// see README "Week 2"). Renamed from `FillCheckStub` in week 2 so that name can carry
/// the deterministic offline classifier below.
#[derive(Debug, Default, Clone, Copy)]
pub struct AlwaysFillStub;

impl FillCheck for AlwaysFillStub {
    fn check(&self, _fill: &SimFill) -> FillStatus {
        FillStatus::Filled
    }
}

/// Week-2 deterministic OFFLINE fill-check (specs/hl-agent.md §Week 2; the engine's
/// venue classifier lives in crates/guardrail/src/fill.rs and needs a venue response —
/// this stub runs with no network and no venue at all):
/// - market order with a mark, or a limit order CROSSING the mark (buy limit >= mark,
///   sell limit <= mark) -> FILLED (at the limit price, matching the simulator's
///   execution-price convention);
/// - a resting (non-crossing) limit -> UNKNOWN ("not done yet", per the engine's
///   conservative semantics — never guessed into a stronger class);
/// - anything else (malformed size/price, market order with no mark) -> FAILED
///   (locally provable "definitely did not happen"; safe to resubmit as a NEW attempt).
/// Pure function of its arguments — same inputs, same output, always.
#[derive(Debug, Default, Clone, Copy)]
pub struct FillCheckStub;

impl FillCheckStub {
    pub fn check_intent(&self, intent: &OrderIntent, mark: Option<f64>) -> FillStatus {
        if !intent.size.is_finite() || intent.size <= 0.0 {
            return FillStatus::Failed; // malformed: can never trade
        }
        match intent.limit_price {
            None => {
                // market: fills at the mark when one exists
                match mark {
                    Some(m) if m.is_finite() && m > 0.0 => FillStatus::Filled,
                    _ => FillStatus::Failed, // no price to cross offline
                }
            }
            Some(limit) => {
                if !limit.is_finite() || limit <= 0.0 {
                    return FillStatus::Failed; // malformed limit price
                }
                match mark {
                    Some(m) if m.is_finite() && m > 0.0 => {
                        let crossing = if intent.is_buy { limit >= m } else { limit <= m };
                        if crossing {
                            FillStatus::Filled // at the limit price
                        } else {
                            FillStatus::Unknown // resting on the book, not done YET
                        }
                    }
                    _ => FillStatus::Unknown, // can't tell crossing without a mark
                }
            }
        }
    }
}

impl FillCheck for FillCheckStub {
    /// Trait seam: a recorded SimFill carries execution (not intent) data, so the
    /// offline classification here is: well-formed recorded execution -> FILLED,
    /// malformed -> FAILED. Use `check_intent` for real market/limit semantics.
    fn check(&self, fill: &SimFill) -> FillStatus {
        if fill.size.is_finite() && fill.size > 0.0 && fill.price.is_finite() && fill.price > 0.0 {
            FillStatus::Filled
        } else {
            FillStatus::Failed
        }
    }
}

#[cfg(test)]
mod fill_stub_tests {
    use super::*;

    fn intent(coin: &str, is_buy: bool, size: f64, limit_price: Option<f64>) -> OrderIntent {
        OrderIntent { coin: coin.into(), is_buy, size, limit_price, reduce_only: false, intent_id: None }
    }

    #[test]
    fn market_and_crossing_limit_fill() {
        let stub = FillCheckStub;
        // market with a mark: FILLED
        assert_eq!(stub.check_intent(&intent("ETH", true, 0.5, None), Some(2000.0)), FillStatus::Filled);
        // buy limit ABOVE the mark crosses: FILLED (at the limit price)
        assert_eq!(stub.check_intent(&intent("ETH", true, 0.5, Some(2010.0)), Some(2000.0)), FillStatus::Filled);
        // sell limit BELOW the mark crosses: FILLED
        assert_eq!(stub.check_intent(&intent("ETH", false, 0.5, Some(1990.0)), Some(2000.0)), FillStatus::Filled);
    }

    #[test]
    fn resting_limit_is_unknown() {
        let stub = FillCheckStub;
        // buy limit BELOW the mark rests on the book: UNKNOWN (not done YET)
        assert_eq!(stub.check_intent(&intent("ETH", true, 0.5, Some(1990.0)), Some(2000.0)), FillStatus::Unknown);
        // sell limit ABOVE the mark rests: UNKNOWN
        assert_eq!(stub.check_intent(&intent("ETH", false, 0.5, Some(2010.0)), Some(2000.0)), FillStatus::Unknown);
        // no mark at all: can't tell crossing -> UNKNOWN (never a confident class)
        assert_eq!(stub.check_intent(&intent("ETH", true, 0.5, Some(2000.0)), None), FillStatus::Unknown);
    }

    #[test]
    fn malformed_or_uncrossable_is_failed() {
        let stub = FillCheckStub;
        // market order with no mark: nothing to cross offline -> FAILED
        assert_eq!(stub.check_intent(&intent("ETH", true, 0.5, None), None), FillStatus::Failed);
        // zero / negative / NaN size -> FAILED
        assert_eq!(stub.check_intent(&intent("ETH", true, 0.0, None), Some(2000.0)), FillStatus::Failed);
        assert_eq!(stub.check_intent(&intent("ETH", true, -1.0, None), Some(2000.0)), FillStatus::Failed);
        assert_eq!(stub.check_intent(&intent("ETH", true, f64::NAN, None), Some(2000.0)), FillStatus::Failed);
        // NaN / non-positive limit price -> FAILED
        assert_eq!(stub.check_intent(&intent("ETH", true, 0.5, Some(f64::NAN)), Some(2000.0)), FillStatus::Failed);
        assert_eq!(stub.check_intent(&intent("ETH", true, 0.5, Some(0.0)), Some(2000.0)), FillStatus::Failed);
    }
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SimFill {
    pub coin: String,
    pub is_buy: bool,
    pub size: f64,
    /// Execution price: mark for market orders, limit price for limit orders.
    pub price: f64,
    pub notional_usd: f64,
    pub ts_ms: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub intent_id: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct SimResult {
    pub decision: Decision,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fill: Option<SimFill>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fill_status: Option<FillStatus>,
    pub position_after: f64,
    pub realized_pnl_today_usd: f64,
}

/// In-memory position book for the dry run.
pub struct DryRunSimulator {
    pub guardrail: Guardrail,
    pub fill_check: Box<dyn FillCheck>,
    positions: HashMap<String, f64>,
    avg_entry: HashMap<String, f64>,
    pub fills: Vec<SimFill>,
}

impl std::fmt::Debug for DryRunSimulator {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DryRunSimulator")
            .field("guardrail", &self.guardrail)
            .field("fills", &self.fills.len())
            .finish_non_exhaustive()
    }
}

impl DryRunSimulator {
    pub fn new(guardrail: Guardrail) -> Self {
        Self { guardrail, fill_check: Box::new(FillCheckStub), positions: HashMap::new(), avg_entry: HashMap::new(), fills: Vec::new() }
    }

    pub fn with_fill_check(mut self, fc: Box<dyn FillCheck>) -> Self {
        self.fill_check = fc;
        self
    }

    pub fn position(&self, coin: &str) -> f64 {
        *self.positions.get(coin).unwrap_or(&0.0)
    }

    /// Gate + execute a dry run. `mark` is the execution price source; None mark with a
    /// market order is rejected (an honest dry run needs a price to fill at).
    pub fn execute(&mut self, intent: &OrderIntent, mark: Option<f64>, now_ms: i64) -> SimResult {
        // position cap needs the current position (the gate itself is position-blind)
        if let Some(mark) = mark {
            if mark.is_finite() && mark > 0.0 {
                let signed = if intent.is_buy { intent.size } else { -intent.size };
                let new_pos = self.position(&intent.coin) + signed;
                let worst = new_pos.abs().max(self.position(&intent.coin).abs()) * mark;
                if worst > self.guardrail.cfg.max_position_notional_usd + 1e-9 {
                    let reason = format!(
                        "position cap: |position| would reach {:.4} {}; position notional {:.2} USD > cap {:.2}",
                        new_pos.abs(), intent.coin, worst, self.guardrail.cfg.max_position_notional_usd
                    );
                    return SimResult {
                        decision: Decision::Reject { reason },
                        fill: None,
                        fill_status: None,
                        position_after: self.position(&intent.coin),
                        realized_pnl_today_usd: self.guardrail.realized_pnl_today(),
                    };
                }
            }
        }

        let decision = self.guardrail.allow(intent, Ctx::simple(now_ms, mark));
        if !decision.is_allow() {
            return SimResult {
                decision,
                fill: None,
                fill_status: None,
                position_after: self.position(&intent.coin),
                realized_pnl_today_usd: self.guardrail.realized_pnl_today(),
            };
        }

        // Simulated fill: market orders fill at the mark; limit orders at the limit price
        // (week-1 stub does not model queueing/partial fills — documented in README).
        let price = match (intent.limit_price, mark) {
            (Some(px), _) => px,
            (None, Some(m)) => m,
            (None, None) => {
                return SimResult {
                    decision: Decision::Reject { reason: "market order needs a mark price; none available".into() },
                    fill: None,
                    fill_status: None,
                    position_after: self.position(&intent.coin),
                    realized_pnl_today_usd: self.guardrail.realized_pnl_today(),
                }
            }
        };
        let fill = SimFill {
            coin: intent.coin.clone(),
            is_buy: intent.is_buy,
            size: intent.size,
            price,
            notional_usd: intent.size * price,
            ts_ms: now_ms,
            intent_id: intent.intent_id.clone(),
        };
        let status = self.fill_check.check(&fill);

        // Book the position + realized PnL only when the fill-check confirms FILLED.
        // PARTIAL/FAILED/UNKNOWN change the book only in the real engine (week 2+); the
        // stub never produces them, so the book stays consistent with its checker.
        if status == FillStatus::Filled {
            let signed = if intent.is_buy { intent.size } else { -intent.size };
            let old_pos = self.position(&intent.coin);
            let new_pos = old_pos + signed;
            let entry = self.avg_entry.get(&intent.coin).copied().unwrap_or(price);
            // realized PnL accrues only on the reducing side
            let pnl = if old_pos > 0.0 && signed < 0.0 {
                (price - entry) * (-signed).min(old_pos)
            } else if old_pos < 0.0 && signed > 0.0 {
                (entry - price) * signed.min(-old_pos)
            } else {
                0.0
            };
            if pnl != 0.0 {
                self.guardrail.record_realized_pnl(pnl, now_ms);
            }
            if new_pos.abs() < 1e-12 {
                self.positions.remove(&intent.coin);
                self.avg_entry.remove(&intent.coin);
            } else {
                if old_pos.signum() == new_pos.signum() && new_pos.abs() > old_pos.abs() {
                    // increasing: volume-weighted average entry
                    let new_entry = if new_pos != 0.0 {
                        (entry * old_pos.abs() + price * intent.size) / new_pos.abs()
                    } else {
                        price
                    };
                    self.avg_entry.insert(intent.coin.clone(), new_entry);
                } else if old_pos.signum() != new_pos.signum() || old_pos == 0.0 {
                    self.avg_entry.insert(intent.coin.clone(), price);
                }
                self.positions.insert(intent.coin.clone(), new_pos);
            }
            self.fills.push(fill.clone());
        }
        SimResult {
            decision,
            fill: Some(fill),
            fill_status: Some(status),
            position_after: self.position(&intent.coin),
            realized_pnl_today_usd: self.guardrail.realized_pnl_today(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::guardrail::GuardrailConfig;

    fn cfg() -> GuardrailConfig {
        GuardrailConfig {
            max_position_notional_usd: 10_000.0,
            max_order_notional_usd: 1_000.0,
            max_leverage: 2.0,
            account_equity_usd: 5_000.0,
            daily_loss_cap_usd: 50.0,
            max_similar_actions: 5,
            loop_window_seconds: 60,
            max_calls_per_hour: 100,
        }
    }

    fn intent(coin: &str, is_buy: bool, size: f64) -> OrderIntent {
        OrderIntent { coin: coin.into(), is_buy, size, limit_price: None, reduce_only: false, intent_id: None }
    }

    #[test]
    fn buy_then_sell_realizes_pnl_and_updates_book() {
        let mut sim = DryRunSimulator::new(Guardrail::new(cfg()));
        let r = sim.execute(&intent("BTC", true, 0.01), Some(80_000.0), 1_000);
        assert!(r.decision.is_allow(), "{r:?}");
        assert_eq!(r.position_after, 0.01);
        assert_eq!(r.fill_status, Some(FillStatus::Filled));
        let r = sim.execute(&intent("BTC", false, 0.01), Some(81_000.0), 2_000);
        assert!(r.decision.is_allow(), "{r:?}");
        assert_eq!(r.position_after, 0.0);
        // realized: (81000-80000)*0.01 = 10 USD
        assert!((r.realized_pnl_today_usd - 10.0).abs() < 1e-6, "{}", r.realized_pnl_today_usd);
    }

    #[test]
    fn position_cap_blocks_when_flat_book_would_breach() {
        // position cap 10_000 USD: 0.2 BTC at 80_000 = 16_000 USD breaches it (and the 1_000 order cap too; the position check runs first).
        let mut sim = DryRunSimulator::new(Guardrail::new(cfg()));
        let r = sim.execute(&intent("BTC", true, 0.2), Some(80_000.0), 1_000);
        assert!(!r.decision.is_allow(), "{r:?}");
        assert!(r.decision.rejected_reason().contains("notional"), "{r:?}");
    }

    #[test]
    fn market_order_without_mark_is_rejected_honestly() {
        let mut sim = DryRunSimulator::new(Guardrail::new(cfg()));
        let r = sim.execute(&intent("BTC", true, 0.01), None, 1_000);
        assert!(!r.decision.is_allow(), "{r:?}");
    }

    #[test]
    fn losing_day_trips_the_cap_through_fills() {
        let mut sim = DryRunSimulator::new(Guardrail::new(cfg()));
        let t = 1_700_000_000_000;
        sim.execute(&intent("BTC", true, 0.01), Some(80_000.0), t).decision.is_allow();
        // dump 52 USD below entry: (74_800 - 80_000)*0.01 = -52
        let r = sim.execute(&intent("BTC", false, 0.01), Some(74_800.0), t + 1_000);
        assert!(r.decision.is_allow(), "{r:?}");
        assert!(r.realized_pnl_today_usd <= -50.0);
        // next order: daily cap hit
        let r = sim.execute(&intent("ETH", true, 0.01), Some(2_000.0), t + 2_000);
        assert!(!r.decision.is_allow(), "{r:?}");
        assert!(r.decision.rejected_reason().contains("daily loss cap"), "{r:?}");
    }

    #[test]
    fn custom_fill_check_short_circuits_the_book() {
        struct NeverFills;
        impl FillCheck for NeverFills {
            fn check(&self, _f: &SimFill) -> FillStatus {
                FillStatus::Failed
            }
        }
        let mut sim = DryRunSimulator::new(Guardrail::new(cfg())).with_fill_check(Box::new(NeverFills));
        let r = sim.execute(&intent("BTC", true, 0.01), Some(80_000.0), 1_000);
        assert!(r.decision.is_allow());
        assert_eq!(r.fill_status, Some(FillStatus::Failed));
        assert_eq!(r.position_after, 0.0, "FAILED fill must not move the book");
        assert!(sim.fills.is_empty());
    }
}
