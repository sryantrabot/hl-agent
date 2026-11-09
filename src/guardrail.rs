//! Guardrail gate — SWAPPED 2026-11-05 (ceo/tech r22, CEO r178): the real engine
//! now lives in `crates/guardrail` (pure, no network, no keys, no clock reads;
//! shared unchanged by Bet A and the Bet B agent-guard add-on). This module is a
//! re-export shim so every existing `crate::guardrail::*` /
//! `hl_agent::guardrail::*` path keeps working. The week-1 inline stub is retired;
//! its behavior (same caps, same rejection order, same reasons) is pinned by the
//! engine's 36-test suite plus this crate's own tests.
pub use ::guardrail::*;
