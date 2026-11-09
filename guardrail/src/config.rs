//! Guardrail caps config: TOML in, validated struct out. Same field names and
//! semantics as the product-side week-1 stub so the swap is drop-in.
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GuardrailConfig {
    /// Max |position| notional per coin, USD.
    pub max_position_notional_usd: f64,
    /// Max order notional, USD.
    pub max_order_notional_usd: f64,
    /// Max leverage: order notional <= leverage * account_equity_usd.
    pub max_leverage: f64,
    /// Account equity assumed for the leverage cap (the caller supplies it; the
    /// engine never queries an account).
    pub account_equity_usd: f64,
    /// Stop for the day when cumulative realized PnL (fed to the gate) <= -this, USD.
    pub daily_loss_cap_usd: f64,
    /// Tripwire: reject when >= max_similar_actions near-identical intents land
    /// inside loop_window_seconds.
    pub max_similar_actions: usize,
    pub loop_window_seconds: i64,
    /// Max order intents per rolling hour (spec: "max calls/hour").
    pub max_calls_per_hour: usize,
}

impl GuardrailConfig {
    /// Parse + validate a TOML string. Returns the human-readable error on failure
    /// (parse errors name the field; validate errors name the cap).
    pub fn from_toml_str(s: &str) -> Result<Self, String> {
        let cfg: Self = toml::from_str(s).map_err(|e| format!("guardrail config parse: {e}"))?;
        cfg.validate().map_err(|e| format!("guardrail config invalid: {e}"))?;
        Ok(cfg)
    }

    /// All caps must be finite and > 0; NaN/inf/zero/negative caps would silently
    /// disable the guard they name (NaN comparisons are always false), so they are
    /// hard config errors.
    pub fn validate(&self) -> Result<(), String> {
        let bad = |name: &str| format!("{name} must be finite and > 0");
        for (name, v) in [
            ("max_position_notional_usd", self.max_position_notional_usd),
            ("max_order_notional_usd", self.max_order_notional_usd),
            ("max_leverage", self.max_leverage),
            ("account_equity_usd", self.account_equity_usd),
            ("daily_loss_cap_usd", self.daily_loss_cap_usd),
        ] {
            if !(v.is_finite() && v > 0.0) {
                return Err(bad(name));
            }
        }
        if self.max_similar_actions == 0 || self.max_calls_per_hour == 0 {
            return Err("max_similar_actions and max_calls_per_hour must be >= 1".into());
        }
        if self.loop_window_seconds <= 0 {
            return Err("loop_window_seconds must be > 0".into());
        }
        Ok(())
    }
}
