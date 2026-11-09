//! # guardrail — the pure guardrail engine (the kit core, the add-on shared)
//!
//! Own: ceo/tech (2026-11-04). Spec: this README "Week 1 (tech)".
//!
//! Every guardrail decision is a pure function of (config, intent, caller-supplied
//! [`Ctx`] / [`FillReport`]): this crate NEVER does I/O, never reads the wall clock,
//! never holds keys, and never talks to a venue. The caller (an MCP server, a bot
//! loop, a hosted tier) supplies the clock and the venue statuses; the engine
//! decides deterministically and explains every rejection with a reason string.
//!
//! Four surfaces:
//! 1. [`GuardrailConfig`] — TOML caps (position notional, order notional, leverage,
//!    daily loss, calls/hour) + loop-tripwire window settings. `deny_unknown_fields`
//!    so a typo in a TOML file is a hard error, not a silently-unenforced cap.
//! 2. [`Guardrail::allow`] — the pre-trade gate: `allow(intent, ctx) -> Decision`
//!    called BEFORE signing/sending. Stateful only in its own sliding windows;
//!    all time comes from `ctx.now_ms`.
//! 3. [`fill`] — "did it really happen": classify an intended order + the venue's
//!    returned status into FILLED / PARTIAL / FAILED / UNKNOWN, deterministically,
//!    with an [`fill::IdempotencyBook`] (never resubmit the same intent id) and a
//!    [`fill::run_with_backoff`] retry helper (the engine never sleeps; the caller
//!    does, via the `on_error` callback).
//!
//! Rejection is the default: any input the engine cannot fully reason about
//! (NaN sizes, unknown statuses, contradictory reports) classifies UNKNOWN or
//! rejects, never allows.
#![forbid(unsafe_code)]

mod config;
mod fill;
mod gate;

pub use config::GuardrailConfig;
pub use fill::{
    classify_fill, new_intent_id, run_with_backoff, BackoffPolicy, FillReport, FillStatus, IntentState,
    RegisterOutcome, RetryOutcome, VenueStatus,
    IdempotencyBook, IntentRecord, PENDING_WINDOW_MS,
};
pub use gate::{bucket, Ctx, Decision, Guardrail, OrderIntent};
