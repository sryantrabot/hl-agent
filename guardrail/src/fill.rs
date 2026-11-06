//! Fill check: "did it really happen?" — the deterministic classifier for an
//! intended order + whatever the venue returned, plus the idempotency book
//! (never resubmit the same intent) and the retry-with-backoff helper.
//!
//! Determinism contract: `classify_fill` is a pure function of its arguments.
//! The engine NEVER sleeps and NEVER does I/O — `run_with_backoff` drives the
//! caller's `attempt` closure and delegates every actual wait to the caller's
//! `on_wait` callback.
use serde::{Deserialize, Serialize};

/// A fill outcome, venue-shaped (SCREAMING_SNAKE_CASE, matching the top-level\n/// crate's `FillStatus` so swaps stay mechanical).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum FillStatus {
    Filled,
    Partial,
    Failed,
    Unknown,
}

impl FillStatus {
    /// True when the order definitely did not happen (safe to resubmit as a new attempt).
    pub fn is_failed(self) -> bool {
        matches!(self, FillStatus::Failed)
    }
    /// True when the order definitely completed at full size.
    pub fn is_filled(self) -> bool {
        matches!(self, FillStatus::Filled)
    }
}

/// The venue status as the caller normalized it. Unknown raw strings stay
/// `Unknown` (never guessed into a stronger class).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VenueStatus {
    /// Venue says the order is completely filled.
    Filled,
    /// Venue says the order is resting/live/working (not done yet).
    Open,
    /// Venue says the order was canceled (possibly after a partial fill).
    Canceled,
    /// Venue refused the order outright.
    Rejected,
    /// Transport/local error: we do not know whether the venue ever saw it.
    Error,
    /// Anything else (the raw string is kept for the audit trail).
    Unknown(String),
}

impl VenueStatus {
    /// Normalize a venue status string (case-insensitive, common synonyms).
    /// Conservative: anything unrecognized is `Unknown(s)`.
    pub fn parse(s: &str) -> Self {
        let t: String = s.trim().to_ascii_lowercase();
        match t.as_str() {
            "filled" | "fill" | "done" => VenueStatus::Filled,
            "open" | "resting" | "live" | "working" | "queued" | "triggered" => VenueStatus::Open,
            "canceled" | "cancelled" | "cancel" => VenueStatus::Canceled,
            "rejected" | "reject" | "margin_canceled" | "margin_canceled_or_rejected" => VenueStatus::Rejected,
            "error" | "transport" | "io" | "timeout" => VenueStatus::Error,
            other => VenueStatus::Unknown(other.to_string()),
        }
    }
}

/// One classification input: what we meant to do, what the venue said, and how
/// much actually filled (base-asset units, same convention as the order).
#[derive(Debug, Clone)]
pub struct FillReport {
    /// Intended base-asset size (the gated intent's size).
    pub intended_size: f64,
    /// Venue-returned status (already normalized, or parse it here from raw).
    pub venue_status: VenueStatus,
    /// Base-asset size the venue reports as filled (0.0 when none reported).
    pub filled_size: f64,
}

/// Relative tolerance for "filled == intended": a venue may report 0.9999999x
/// after rounding. `full` means filled + eps >= intended, eps = 1e-9 * max(intended, 1).
fn is_full(intended: f64, filled: f64) -> bool {
    let eps = 1e-9 * (intended.abs().max(1.0));
    filled + eps >= intended
}

/// Classify (intended order, venue response) -> FILLED / PARTIAL / FAILED / UNKNOWN.
///
/// The filled size is authoritative over the status string when they disagree
/// (sizes are measurements; statuses are labels):
/// - a status "filled" with a smaller filled size is PARTIAL;
/// - a cancel with a partial fill is PARTIAL, a cancel with zero fill is FAILED;
/// - an ERROR (transport) with zero reported fill is UNKNOWN — the order may
///   have landed; that is exactly the case idempotency + retry exist for.
/// Contradictions and invalid inputs (NaN, negative, zero intended size)
/// classify UNKNOWN — never a confident class.
pub fn classify_fill(report: &FillReport) -> FillStatus {
    if !(report.intended_size.is_finite() && report.intended_size > 0.0) {
        return FillStatus::Unknown;
    }
    if !report.filled_size.is_finite() || report.filled_size < 0.0 {
        return FillStatus::Unknown;
    }
    let filled = report.filled_size;
    match &report.venue_status {
        VenueStatus::Filled => {
            if is_full(report.intended_size, filled) {
                FillStatus::Filled
            } else if filled > 0.0 {
                FillStatus::Partial
            } else {
                FillStatus::Unknown // status says filled, size says nothing happened
            }
        }
        VenueStatus::Open => {
            if is_full(report.intended_size, filled) {
                FillStatus::Filled // size is authoritative; the status string is stale
            } else if filled > 0.0 {
                FillStatus::Partial
            } else {
                FillStatus::Unknown // resting, nothing filled YET
            }
        }
        VenueStatus::Canceled => {
            if is_full(report.intended_size, filled) {
                FillStatus::Filled // cancel raced the fill; the size wins
            } else if filled > 0.0 {
                FillStatus::Partial
            } else {
                FillStatus::Failed
            }
        }
        VenueStatus::Rejected => {
            if filled > 0.0 {
                FillStatus::Unknown // rejected yet filled: contradiction
            } else {
                FillStatus::Failed
            }
        }
        VenueStatus::Error => {
            if is_full(report.intended_size, filled) {
                FillStatus::Filled
            } else if filled > 0.0 {
                FillStatus::Partial
            } else {
                FillStatus::Unknown // did the venue even see it? genuinely unknown
            }
        }
        VenueStatus::Unknown(_) => {
            if is_full(report.intended_size, filled) {
                FillStatus::Filled
            } else if filled > 0.0 {
                FillStatus::Partial
            } else {
                FillStatus::Unknown
            }
        }
    }
}

/// Build a correlation id from caller-supplied entropy (seed + time). The engine
/// itself never reads the clock or a RNG — the caller feeds both, so tests and
/// replays stay deterministic.
pub fn new_intent_id(coin: &str, seed: u64, ts_ms: i64) -> String {
    format!("{coin}-{seed:016x}-{ts_ms}")
}

/// Lifecycle of one intent id in the book.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IntentState {
    /// Submitted, outcome not yet known (must never be resubmitted).
    Pending,
    /// Known terminal outcome.
    Settled(FillStatus),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IntentRecord {
    pub state: IntentState,
    pub first_seen_ms: i64,
    pub last_update_ms: i64,
}

/// What `register` decided: whether the caller may submit this intent id.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RegisterOutcome {
    /// Fresh id (or a previously FAILED one): submit is allowed.
    Fresh,
    /// The same id is already in flight: DO NOT resubmit.
    DuplicatePending,
    /// The same id already settled with this outcome: DO NOT resubmit.
    DuplicateSettled(FillStatus),
}

/// Idempotency book: intent id -> record. Pure in-memory; a hosted tier would
/// persist it, but the logic is identical. Bounded: at capacity the oldest
/// SETTLED record is evicted first, then the oldest record overall.
#[derive(Debug, Default)]
pub struct IdempotencyBook {
    records: Vec<(String, IntentRecord)>,
    capacity: usize,
}

/// Pending records older than this (caller-supplied clock) are treated as
/// expired on register: the id becomes usable again with `Fresh`. One minute:
/// long enough to dedupe a retry storm, short enough that a crashed submitter
/// does not wedge the book.
pub const PENDING_WINDOW_MS: i64 = 60_000;

impl IdempotencyBook {
    pub fn new() -> Self {
        Self { records: Vec::new(), capacity: 4096 }
    }

    pub fn with_capacity(capacity: usize) -> Self {
        Self { records: Vec::new(), capacity: capacity.max(1) }
    }

    pub fn len(&self) -> usize {
        self.records.len()
    }

    pub fn is_empty(&self) -> bool {
        self.records.is_empty()
    }

    pub fn get(&self, intent_id: &str) -> Option<&IntentRecord> {
        self.records.iter().find(|(id, _)| id == intent_id).map(|(_, r)| r)
    }

    /// Decide whether this intent id may be submitted now. FAILED records are
    /// removed (a new submit is a genuinely new attempt). Expired PENDING
    /// records (older than PENDING_WINDOW_MS by the caller's clock) are also
    /// freed — the caller then treats the resubmit as a new idempotent attempt.
    pub fn register(&mut self, intent_id: &str, now_ms: i64) -> RegisterOutcome {
        let idx = self.records.iter().position(|(id, _)| id == intent_id);
        if let Some(i) = idx {
            let rec = &self.records[i].1;
            match &rec.state {
                IntentState::Settled(s) => return RegisterOutcome::DuplicateSettled(*s),
                IntentState::Pending => {
                    let age = (now_ms - rec.first_seen_ms).max(0);
                    if age < PENDING_WINDOW_MS {
                        return RegisterOutcome::DuplicatePending;
                    }
                    self.records.remove(i); // expired: freed for a fresh attempt
                }
            }
        }
        self.records
            .push((intent_id.to_string(), IntentRecord { state: IntentState::Pending, first_seen_ms: now_ms, last_update_ms: now_ms }));
        self.evict();
        RegisterOutcome::Fresh
    }

    /// Remove a FAILED record (after `classify_fill` returned FAILED) so the id
    /// may be reused for the next attempt.
    pub fn mark_failed(&mut self, intent_id: &str) -> Result<(), String> {
        let i = self
            .records
            .iter()
            .position(|(id, _)| id == intent_id)
            .ok_or_else(|| format!("unknown intent id {intent_id}"))?;
        self.records.remove(i);
        Ok(())
    }

    /// Record the terminal outcome of an intent id.
    pub fn mark_settled(&mut self, intent_id: &str, status: FillStatus, now_ms: i64) -> Result<(), String> {
        let rec = self
            .records
            .iter_mut()
            .find(|(id, _)| id == intent_id)
            .map(|(_, r)| r)
            .ok_or_else(|| format!("unknown intent id {intent_id}"))?;
        rec.state = IntentState::Settled(status);
        rec.last_update_ms = now_ms;
        Ok(())
    }

    fn evict(&mut self) {
        while self.records.len() > self.capacity {
            let oldest_settled = self.records.iter().position(|(_, r)| matches!(r.state, IntentState::Settled(_)));
            let i = oldest_settled.unwrap_or(0);
            self.records.remove(i);
        }
    }
}

/// Retry policy for `run_with_backoff`. All fields validated: max_attempts >= 1,
/// base_ms > 0, max_ms >= base_ms.
#[derive(Debug, Clone, Copy)]
pub struct BackoffPolicy {
    pub max_attempts: u32,
    pub base_ms: u64,
    pub max_ms: u64,
}

impl BackoffPolicy {
    pub fn validate(&self) -> Result<(), String> {
        if self.max_attempts == 0 {
            return Err("max_attempts must be >= 1".into());
        }
        if self.base_ms == 0 {
            return Err("base_ms must be > 0".into());
        }
        if self.max_ms < self.base_ms {
            return Err("max_ms must be >= base_ms".into());
        }
        Ok(())
    }

    /// Deterministic exponential delay before attempt `attempt` (1-based):
    /// base * 2^(attempt-1), capped at max_ms. No jitter (jitter needs a RNG).
    pub fn delay_ms(&self, attempt: u32) -> u64 {
        let exp = attempt.saturating_sub(1).min(16);
        self.base_ms.saturating_mul(1u64 << exp).min(self.max_ms)
    }
}

/// Outcome of `run_with_backoff`.
#[derive(Debug)]
pub enum RetryOutcome<E> {
    /// An attempt produced a report; here is its classified outcome.
    Done { status: FillStatus, attempts: u32 },
    /// Every attempt errored before producing a report (nothing to classify).
    Exhausted { attempts: u32, last_error: E },
}

/// Retry an order submission with exponential backoff, WITHOUT the engine ever
/// sleeping or doing I/O: `attempt(attempt_no)` tries once (network etc.),
/// `on_wait(delay_ms)` is called before each retry and the CALLER sleeps there
/// (or records the delay, or skips sleeping in tests).
///
/// Idempotency belongs to the caller: wrap the intent id in
/// `IdempotencyBook::register` before the first `run_with_backoff` so a retry
/// after an UNKNOWN never double-submits the same id.
pub fn run_with_backoff<A, W, E>(policy: BackoffPolicy, mut attempt: A, mut on_wait: W) -> Result<RetryOutcome<E>, String>
where
    A: FnMut(u32) -> Result<FillReport, E>,
    W: FnMut(u64),
{
    policy.validate()?;
    let mut n: u32 = 0;
    loop {
        n += 1;
        match attempt(n) {
            Ok(report) => {
                let status = classify_fill(&report);
                return Ok(RetryOutcome::Done { status, attempts: n });
            }
            Err(e) => {
                if n >= policy.max_attempts {
                    return Ok(RetryOutcome::Exhausted { attempts: n, last_error: e });
                }
                on_wait(policy.delay_ms(n));
            }
        }
    }
}
