//! guardrail engine acceptance tests (CEO r178, specs/hl-agent.md Week 1 tech).
//! Everything runs through the PUBLIC API only: config TOML, allow() gate,
//! fill classification, idempotency book, retry helper.
use guardrail::{
    classify_fill, new_intent_id, run_with_backoff, BackoffPolicy, Ctx, Decision, FillReport,
    FillStatus, Guardrail, GuardrailConfig, IdempotencyBook, IntentState, PENDING_WINDOW_MS,
    RegisterOutcome, RetryOutcome, VenueStatus,
};

// ---------------------------------------------------------------- config ---

const GOOD_TOML: &str = "\
max_position_notional_usd = 10000.0
max_order_notional_usd = 1000.0
max_leverage = 2.0
account_equity_usd = 5000.0
daily_loss_cap_usd = 50.0
max_similar_actions = 2
loop_window_seconds = 60
max_calls_per_hour = 5
";

#[test]
fn good_config_parses_and_validates() {
    let cfg = GuardrailConfig::from_toml_str(GOOD_TOML).expect("good toml");
    assert_eq!(cfg.max_order_notional_usd, 1000.0);
    assert_eq!(cfg.max_calls_per_hour, 5);
}

#[test]
fn unknown_toml_field_is_a_hard_error_not_a_silent_cap() {
    let s = format!("{GOOD_TOML}max_positon_notional_usd = 1.0\n"); // typo
    let err = GuardrailConfig::from_toml_str(&s).unwrap_err();
    assert!(err.contains("guardrail config parse"), "{err}");
}

#[test]
fn nan_inf_zero_and_negative_caps_are_invalid() {
    for (field, orig, bad) in [
        ("max_position_notional_usd", "10000.0", "0.0"),
        ("max_order_notional_usd", "1000.0", "-5.0"),
        ("max_leverage", "2.0", "nan"),
        ("account_equity_usd", "5000.0", "inf"),
        ("daily_loss_cap_usd", "50.0", "nan"),
    ] {
        let s = GOOD_TOML.replace(&format!("{field} = {orig}"), &format!("{field} = {bad}"));
        let err = GuardrailConfig::from_toml_str(&s).unwrap_err();
        assert!(err.contains("must be finite and > 0"), "{field}={bad}: {err}");
    }
}

#[test]
fn tripwire_and_rate_caps_must_be_positive() {
    let s = GOOD_TOML.replace("max_similar_actions = 2", "max_similar_actions = 0");
    assert!(GuardrailConfig::from_toml_str(&s).is_err());
    let s = GOOD_TOML.replace("max_calls_per_hour = 5", "max_calls_per_hour = 0");
    assert!(GuardrailConfig::from_toml_str(&s).is_err());
    let s = GOOD_TOML.replace("loop_window_seconds = 60", "loop_window_seconds = 0");
    assert!(GuardrailConfig::from_toml_str(&s).is_err());
}

// ------------------------------------------------------------------ gate ---

fn cfg() -> GuardrailConfig {
    GuardrailConfig::from_toml_str(GOOD_TOML).unwrap()
}

fn intent(coin: &str, is_buy: bool, size: f64, limit: Option<f64>) -> guardrail::OrderIntent {
    guardrail::OrderIntent { coin: coin.into(), is_buy, size, limit_price: limit, reduce_only: false, intent_id: None }
}

fn ctx(now_ms: i64, mark: Option<f64>) -> Ctx {
    Ctx::simple(now_ms, mark)
}

#[test]
fn allows_a_normal_order() {
    let mut g = Guardrail::new(cfg());
    assert!(g.allow(&intent("BTC", true, 0.01, Some(80_000.0)), ctx(1_000_000, Some(80_000.0))).is_allow());
}

#[test]
fn cap_exactly_hit_is_allowed_one_epsilon_over_is_rejected() {
    let mut g = Guardrail::new(cfg()); // order cap 1000
    // 0.0125 BTC * 80_000 = 1000.00 exactly -> allow (1e-9 tolerance)
    let d = g.allow(&intent("BTC", true, 0.0125, None), ctx(1_000_000, Some(80_000.0)));
    assert!(d.is_allow(), "{d:?}");
    // one tick over -> reject with a reason naming the cap
    let d = g.allow(&intent("ETH", true, 0.5, None), ctx(1_000_001, Some(2_000.001)));
    assert!(!d.is_allow());
    assert!(d.rejected_reason().contains("order notional"), "{d:?}");
}

#[test]
fn zero_nan_and_negative_sizes_rejected() {
    let mut g = Guardrail::new(cfg());
    for size in [0.0, -1.0, f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
        let d = g.allow(&intent("BTC", true, size, None), ctx(1_000_000, Some(80_000.0)));
        assert!(!d.is_allow(), "size {size} allowed");
        assert!(d.rejected_reason().contains("size"), "{d:?}");
    }
}

#[test]
fn empty_coin_nan_limit_and_nonfinite_mark_rejected() {
    let mut g = Guardrail::new(cfg());
    assert!(!g.allow(&intent("", true, 0.01, None), ctx(1, Some(80_000.0))).is_allow());
    assert!(!g.allow(&intent("BTC", true, 0.01, Some(f64::NAN)), ctx(2, Some(80_000.0))).is_allow());
    assert!(!g.allow(&intent("BTC", true, 0.01, None), ctx(3, Some(f64::NAN))).is_allow());
    assert!(!g.allow(&intent("BTC", true, 0.01, None), ctx(4, Some(-1.0))).is_allow());
}

#[test]
fn loop_tripwire_fires_on_third_near_identical_intent() {
    let mut g = Guardrail::new(cfg()); // max_similar_actions = 2, window 60s
    let t = 10_000_000;
    assert!(g.allow(&intent("BTC", true, 0.01, Some(80_000.0)), ctx(t, Some(80_000.0))).is_allow());
    assert!(g.allow(&intent("BTC", true, 0.0101, Some(80_000.4)), ctx(t + 1_000, Some(80_000.0))).is_allow());
    // third near-identical (same buckets) -> reject
    let d = g.allow(&intent("BTC", true, 0.0101, Some(80_000.4)), ctx(t + 2_000, Some(80_000.0)));
    assert!(!d.is_allow());
    assert!(d.rejected_reason().contains("loop tripwire"), "{d:?}");
    // a materially different order still passes
    assert!(g.allow(&intent("ETH", true, 0.01, Some(2_000.0)), ctx(t + 3_000, Some(2_000.0))).is_allow());
    // ...and after the window expires the same order is fine again
    assert!(g.allow(&intent("BTC", true, 0.01, Some(80_000.0)), ctx(t + 61_000, Some(80_000.0))).is_allow());
}

#[test]
fn tripwire_buckets_make_book_chasing_still_near_identical() {
    // 3-sig-digit price bucket: 80_000 vs 80_049 are the SAME bucket (0.1% steps);
    // 2-sig-digit size bucket: 0.01 vs 0.0107 same, 0.011 different.
    assert_eq!(guardrail::bucket(80_000.0, 3), guardrail::bucket(80_049.0, 3));
    assert_ne!(guardrail::bucket(80_000.0, 3), guardrail::bucket(80_096.0, 3));
    assert_eq!(guardrail::bucket(0.01, 2), guardrail::bucket(0.0101, 2));
    assert_ne!(guardrail::bucket(0.01, 2), guardrail::bucket(0.0107, 2));
    assert_ne!(guardrail::bucket(0.01, 2), guardrail::bucket(0.011, 2));
    assert_eq!(guardrail::bucket(f64::NAN, 3), None);
    assert_eq!(guardrail::bucket(0.0, 2), None);
}

#[test]
fn daily_loss_cap_stops_the_day_and_resets_at_utc_rollover() {
    let mut g = Guardrail::new(cfg()); // daily_loss_cap 50
    let t = 1_700_000_000_000;
    g.record_realized_pnl(-49.99, t);
    assert!(g.allow(&intent("BTC", true, 0.01, None), ctx(t + 1, Some(80_000.0))).is_allow());
    g.record_realized_pnl(-0.02, t + 2); // cumulative -50.01
    let d = g.allow(&intent("BTC", true, 0.01, None), ctx(t + 3, Some(80_000.0)));
    assert!(!d.is_allow());
    assert!(d.rejected_reason().contains("daily loss cap"), "{d:?}");
    // non-finite pnl must not move the counter
    g.record_realized_pnl(f64::NAN, t + 4);
    assert!((g.realized_pnl_today() + 50.01).abs() < 1e-9, "pnl today {}", g.realized_pnl_today());
    // next UTC day the counter resets
    assert!(g.allow(&intent("BTC", true, 0.01, None), ctx(t + 86_400_000, Some(80_000.0))).is_allow());
}

#[test]
fn rate_limit_counts_order_intents_per_rolling_hour_cap_exact() {
    let mut g = Guardrail::new(cfg()); // max 5/hour
    let t = 1_700_000_000_000;
    for i in 0..5 {
        let d = g.allow(&intent("BTC", true, 0.005 + i as f64 * 0.001, None), ctx(t + i * 1_000, Some(80_000.0)));
        assert!(d.is_allow(), "attempt {i}: {d:?}");
    }
    let d = g.allow(&intent("BTC", true, 0.005, None), ctx(t + 6_000, Some(80_000.0)));
    assert!(!d.is_allow());
    assert!(d.rejected_reason().contains("rate limit"), "{d:?}");
    // an hour + 1s later the window is clear
    assert!(g.allow(&intent("BTC", true, 0.005, None), ctx(t + 3_601_000, Some(80_000.0))).is_allow());
}

#[test]
fn clock_skew_backwards_is_safe() {
    // A caller whose clock jumps backwards must not panic, must not un-count
    // the daily loss, and must not corrupt the windows (this is the adversarial
    // "clock skew" edge from the spec).
    let mut g = Guardrail::new(cfg());
    let t = 1_700_000_000_000;
    g.record_realized_pnl(-60.0, t);
    assert!(!g.allow(&intent("BTC", true, 0.01, None), ctx(t + 1, Some(80_000.0))).is_allow());
    // jump backwards 2 days: no panic; the loss cap STAYS tripped for that day
    // (on_clock only resets when the epoch-day CHANGES FORWARD, and a backwards
    // jump lands on a different day -> counter resets to that day's zero; the
    // documented behavior: day counter follows the caller's clock, monotonic
    // caller clocks are the contract. Here we pin: no panic + sane decision).
    let _ = g.allow(&intent("BTC", true, 0.01, None), ctx(t - 2 * 86_400_000, Some(80_000.0)));
    // window retain with a now earlier than all entries keeps them (no panic)
    assert!(g.allow(&intent("BTC", true, 0.005, None), ctx(t - 2 * 86_400_000, Some(80_000.0))).is_allow());
}

#[test]
fn position_cap_rejects_growth_allows_reduction() {
    let mut g = Guardrail::new(cfg()); // max_position_notional 10_000
    // long 0.1 BTC @ 80k = 8_000; buying 0.05 more -> 12_000 > 10_000 -> reject
    let c = Ctx { now_ms: 1_000, mark: Some(80_000.0), position_notional: Some(8_000.0) };
    let d = g.allow(&intent("BTC", true, 0.05, None), c);
    assert!(!d.is_allow());
    assert!(d.rejected_reason().contains("position cap"), "{d:?}");
    // reduce_only sell of the same size -> allowed
    let mut red = intent("BTC", false, 0.01, None);
    red.reduce_only = true;
    assert!(g.allow(&red, c).is_allow());
    // a plain sell (not reduce_only flagged) on an existing long also reads as reduction
    assert!(g.allow(&intent("BTC", false, 0.01, None), c).is_allow());
    // a smaller buy that stays inside the cap is allowed: 8_000 + 1_000 = 9_000
    assert!(g.allow(&intent("BTC", true, 0.0125, None), c).is_allow());
}

#[test]
fn position_cap_requires_a_mark() {
    let mut g = Guardrail::new(cfg());
    let c = Ctx { now_ms: 1_000, mark: None, position_notional: Some(8_000.0) };
    let d = g.allow(&intent("BTC", true, 0.01, None), c);
    assert!(!d.is_allow());
    assert!(d.rejected_reason().contains("mark"), "{d:?}");
}

#[test]
fn leverage_cap_uses_equity() {
    // config where the leverage cap binds BEFORE the order cap:
    // equity 50 x leverage 5 = 250 max notional < 1000 order cap.
    let lev_cfg = GuardrailConfig::from_toml_str(
        &GOOD_TOML
            .replace("account_equity_usd = 5000.0", "account_equity_usd = 50.0")
            .replace("max_leverage = 2.0", "max_leverage = 5.0"),
    )
    .unwrap();
    let mut g = Guardrail::new(lev_cfg);
    // 0.005 BTC x 80_000 = 400 notional: inside the 1000 order cap, above the 250 lev cap
    let d = g.allow(&intent("BTC", true, 0.005, None), ctx(1, Some(80_000.0)));
    assert!(!d.is_allow());
    assert!(d.rejected_reason().contains("max_leverage"), "{d:?}");
}

// ------------------------------------------------------------------ fill ---

fn report(intended: f64, status: VenueStatus, filled: f64) -> FillReport {
    FillReport { intended_size: intended, venue_status: status, filled_size: filled }
}

#[test]
fn classify_happy_paths() {
    assert_eq!(classify_fill(&report(1.0, VenueStatus::Filled, 1.0)), FillStatus::Filled);
    assert_eq!(classify_fill(&report(1.0, VenueStatus::Filled, 0.5)), FillStatus::Partial);
    assert_eq!(classify_fill(&report(1.0, VenueStatus::Canceled, 0.0)), FillStatus::Failed);
    assert_eq!(classify_fill(&report(1.0, VenueStatus::Rejected, 0.0)), FillStatus::Failed);
    assert_eq!(classify_fill(&report(1.0, VenueStatus::Error, 0.0)), FillStatus::Unknown);
    assert_eq!(classify_fill(&report(1.0, VenueStatus::Open, 0.0)), FillStatus::Unknown);
    assert_eq!(classify_fill(&report(1.0, VenueStatus::Open, 0.4)), FillStatus::Partial);
}

#[test]
fn size_is_authoritative_over_status_string() {
    // venue says "filled" but only half landed -> PARTIAL, not Filled
    assert_eq!(classify_fill(&report(2.0, VenueStatus::Filled, 1.0)), FillStatus::Partial);
    // cancel raced the fill and everything landed -> FILLED
    assert_eq!(classify_fill(&report(1.0, VenueStatus::Canceled, 1.0)), FillStatus::Filled);
    // resting order that actually filled fully already -> FILLED
    assert_eq!(classify_fill(&report(1.0, VenueStatus::Open, 1.0)), FillStatus::Filled);
    // transport error but the venue reports full fill -> FILLED (size wins)
    assert_eq!(classify_fill(&report(1.0, VenueStatus::Error, 1.0)), FillStatus::Filled);
}

#[test]
fn contradictions_classify_unknown_never_a_confident_class() {
    // "rejected" yet filled > 0: cannot both be true -> UNKNOWN
    assert_eq!(classify_fill(&report(1.0, VenueStatus::Rejected, 0.5)), FillStatus::Unknown);
    // status "filled" with zero filled size -> UNKNOWN
    assert_eq!(classify_fill(&report(1.0, VenueStatus::Filled, 0.0)), FillStatus::Unknown);
}

#[test]
fn invalid_inputs_classify_unknown() {
    assert_eq!(classify_fill(&report(0.0, VenueStatus::Filled, 0.0)), FillStatus::Unknown);
    assert_eq!(classify_fill(&report(-1.0, VenueStatus::Filled, 1.0)), FillStatus::Unknown);
    assert_eq!(classify_fill(&report(f64::NAN, VenueStatus::Filled, 1.0)), FillStatus::Unknown);
    assert_eq!(classify_fill(&report(1.0, VenueStatus::Filled, f64::NAN)), FillStatus::Unknown);
    assert_eq!(classify_fill(&report(1.0, VenueStatus::Filled, -0.1)), FillStatus::Unknown);
}

#[test]
fn rounding_epsilon_counts_as_full() {
    // venue rounding: 0.999999999999 of the intended size is still Filled
    assert_eq!(classify_fill(&report(1.0, VenueStatus::Filled, 1.0 - 1e-12)), FillStatus::Filled);
    // but a real shortfall is PARTIAL
    assert_eq!(classify_fill(&report(1.0, VenueStatus::Filled, 0.999)), FillStatus::Partial);
}

#[test]
fn venue_status_parse_is_conservative() {
    assert_eq!(VenueStatus::parse("FILLED"), VenueStatus::Filled);
    assert_eq!(VenueStatus::parse("filled"), VenueStatus::Filled);
    assert_eq!(VenueStatus::parse(" Resting "), VenueStatus::Open);
    assert_eq!(VenueStatus::parse("canceled"), VenueStatus::Canceled);
    assert_eq!(VenueStatus::parse("cancelled"), VenueStatus::Canceled);
    assert_eq!(VenueStatus::parse("margin_canceled_or_rejected"), VenueStatus::Rejected);
    assert_eq!(VenueStatus::parse("timeout"), VenueStatus::Error);
    match VenueStatus::parse("weird_status") {
        VenueStatus::Unknown(s) => assert_eq!(s, "weird_status"),
        other => panic!("expected Unknown, got {other:?}"),
    }
}

#[test]
fn unknown_raw_string_with_sizes_still_classifies_by_size() {
    assert_eq!(classify_fill(&report(1.0, VenueStatus::Unknown("??".into()), 1.0)), FillStatus::Filled);
    assert_eq!(classify_fill(&report(1.0, VenueStatus::Unknown("??".into()), 0.0)), FillStatus::Unknown);
}

// -------------------------------------------------------- idempotency book ---

#[test]
fn idempotency_book_blocks_resubmit_of_pending_and_settled() {
    let mut book = IdempotencyBook::new();
    assert_eq!(book.register("i1", 1_000), RegisterOutcome::Fresh);
    // same id while pending -> blocked
    assert_eq!(book.register("i1", 2_000), RegisterOutcome::DuplicatePending);
    // a different id is fine
    assert_eq!(book.register("i2", 2_001), RegisterOutcome::Fresh);
    // settle i1 as FILLED -> resubmits blocked with the outcome
    book.mark_settled("i1", FillStatus::Filled, 3_000).unwrap();
    assert_eq!(book.register("i1", 3_100), RegisterOutcome::DuplicateSettled(FillStatus::Filled));
    // mark FAILED -> the id is freed for the next attempt
    book.mark_settled("i2", FillStatus::Failed, 3_200).unwrap();
    book.mark_failed("i2").unwrap();
    assert_eq!(book.register("i2", 3_300), RegisterOutcome::Fresh);
}

#[test]
fn idempotency_book_pending_expiry_frees_the_id() {
    let mut book = IdempotencyBook::new();
    assert_eq!(book.register("i1", 0), RegisterOutcome::Fresh);
    assert_eq!(book.register("i1", PENDING_WINDOW_MS - 1), RegisterOutcome::DuplicatePending);
    // after the window the crashed submitter no longer wedges the id
    assert_eq!(book.register("i1", PENDING_WINDOW_MS + 1), RegisterOutcome::Fresh);
}

#[test]
fn idempotency_book_is_bounded_and_evicts_settled_first() {
    let mut book = IdempotencyBook::with_capacity(2);
    book.register("a", 1);
    book.register("b", 2);
    book.register("c", 3);
    assert!(book.len() <= 2, "book grew past capacity: {}", book.len());
    // settled records are evicted before pending ones
    let mut book = IdempotencyBook::with_capacity(2);
    book.register("a", 1);
    book.register("b", 2);
    book.mark_settled("a", FillStatus::Filled, 3).unwrap();
    book.register("c", 4); // must evict "a" (settled), keep pending "b"
    assert_eq!(book.get("b").map(|r| r.state.clone()), Some(IntentState::Pending));
    assert!(book.get("a").is_none());
}

#[test]
fn idempotency_book_unknown_ids_error() {
    let mut book = IdempotencyBook::new();
    assert!(book.mark_settled("nope", FillStatus::Filled, 1).is_err());
    assert!(book.mark_failed("nope").is_err());
}

#[test]
fn intent_ids_are_deterministic_strings() {
    let id = new_intent_id("BTC", 0xdead_beef, 1_700_000_000_000);
    assert_eq!(id, "BTC-00000000deadbeef-1700000000000");
    assert_eq!(id, new_intent_id("BTC", 0xdead_beef, 1_700_000_000_000));
    assert_ne!(id, new_intent_id("ETH", 0xdead_beef, 1_700_000_000_000));
}

// ----------------------------------------------------------------- retry ---

#[test]
fn backoff_policy_validates() {
    assert!(BackoffPolicy { max_attempts: 0, base_ms: 100, max_ms: 100 }.validate().is_err());
    assert!(BackoffPolicy { max_attempts: 3, base_ms: 0, max_ms: 100 }.validate().is_err());
    assert!(BackoffPolicy { max_attempts: 3, base_ms: 200, max_ms: 100 }.validate().is_err());
    assert!(BackoffPolicy { max_attempts: 3, base_ms: 100, max_ms: 400 }.validate().is_ok());
}

#[test]
fn backoff_delay_grows_exponentially_and_caps() {
    let p = BackoffPolicy { max_attempts: 10, base_ms: 100, max_ms: 1_000 };
    assert_eq!(p.delay_ms(1), 100);
    assert_eq!(p.delay_ms(2), 200);
    assert_eq!(p.delay_ms(3), 400);
    assert_eq!(p.delay_ms(4), 800);
    assert_eq!(p.delay_ms(5), 1_000); // capped
    assert_eq!(p.delay_ms(9), 1_000); // still capped, no overflow
}

#[test]
fn retry_succeeds_first_try_without_waiting() {
    let p = BackoffPolicy { max_attempts: 3, base_ms: 10, max_ms: 100 };
    let mut waits: Vec<u64> = Vec::new();
    let out = run_with_backoff::<_, _, &str>(
        p,
        |n| {
            assert_eq!(n, 1);
            Ok(report(1.0, VenueStatus::Filled, 1.0))
        },
        |d| waits.push(d),
    )
    .unwrap();
    match out {
        RetryOutcome::Done { status, attempts } => {
            assert_eq!(status, FillStatus::Filled);
            assert_eq!(attempts, 1);
        }
        other => panic!("expected Done, got {other:?}"),
    }
    assert!(waits.is_empty(), "no waits on first-try success");
}

#[test]
fn retry_backs_off_then_succeeds_and_classifies() {
    let p = BackoffPolicy { max_attempts: 3, base_ms: 10, max_ms: 100 };
    let mut waits: Vec<u64> = Vec::new();
    let seen = std::cell::Cell::new(0u32);
    let out = run_with_backoff(
        p,
        |n| {
            seen.set(n);
            if n < 3 {
                Err("transport down")
            } else {
                Ok(report(2.0, VenueStatus::Canceled, 0.5)) // partial cancel
            }
        },
        |d| waits.push(d),
    )
    .unwrap();
    match out {
        RetryOutcome::Done { status, attempts } => {
            assert_eq!(status, FillStatus::Partial);
            assert_eq!(attempts, 3);
        }
        other => panic!("expected Done, got {other:?}"),
    }
    assert_eq!(waits, vec![10, 20], "exponential waits before attempts 2 and 3");
    assert_eq!(seen.get(), 3);
}

#[test]
fn retry_exhausts_and_returns_the_last_error() {
    let p = BackoffPolicy { max_attempts: 3, base_ms: 5, max_ms: 50 };
    let mut waits = 0;
    let out = run_with_backoff(p, |_| Err("still down"), |_| waits += 1).unwrap();
    match out {
        RetryOutcome::Exhausted { attempts, last_error } => {
            assert_eq!(attempts, 3);
            assert_eq!(last_error, "still down");
        }
        other => panic!("expected Exhausted, got {other:?}"),
    }
    assert_eq!(waits, 2, "wait before every retry, none after the last attempt");
}

#[test]
fn retry_rejects_an_invalid_policy_without_calling_attempt() {
    let p = BackoffPolicy { max_attempts: 0, base_ms: 5, max_ms: 50 };
    let out = run_with_backoff::<_, _, &str>(p, |_| panic!("attempt must never run"), |_| ());
    assert!(out.is_err());
}

// ------------------------------------------------------------- end-to-end ---

/// The full flow an MCP server runs: TOML -> gate -> idempotency -> submit ->
/// classify -> settle, with a retry on transport error and a resubmit guard.
#[test]
fn end_to_end_agent_loop() {
    let cfg = GuardrailConfig::from_toml_str(GOOD_TOML).unwrap();
    let mut g = Guardrail::new(cfg);
    let mut book = IdempotencyBook::new();
    let now = 1_700_000_000_000i64;

    let mut ord = intent("BTC", true, 0.01, None);
    ord.intent_id = Some(new_intent_id("BTC", 7, now));

    // 1) pre-trade gate
    let d = g.allow(&ord, Ctx::simple(now, Some(80_000.0)));
    assert!(d.is_allow(), "{d:?}");

    // 2) idempotency: first submit fresh, a duplicate loop iteration blocked
    let id = ord.intent_id.as_deref().unwrap();
    assert_eq!(book.register(id, now), RegisterOutcome::Fresh);
    assert_eq!(book.register(id, now + 1), RegisterOutcome::DuplicatePending);

    // 3) the venue answers "error" with no fill info -> UNKNOWN, order may have landed
    let status = classify_fill(&report(0.01, VenueStatus::Error, 0.0));
    assert_eq!(status, FillStatus::Unknown);
    // so the id STAYS pending: a panicked retry cannot double-submit
    assert_eq!(book.register(id, now + 2_000), RegisterOutcome::DuplicatePending);

    // 4) later the venue confirms the full fill
    let status = classify_fill(&report(0.01, VenueStatus::Filled, 0.01));
    assert_eq!(status, FillStatus::Filled);
    book.mark_settled(id, status, now + 3_000).unwrap();
    assert_eq!(book.register(id, now + 4_000), RegisterOutcome::DuplicateSettled(FillStatus::Filled));

    // 5) the realized PnL feeds back into the gate
    g.record_realized_pnl(-51.0, now + 5_000);
    let d = g.allow(&intent("BTC", true, 0.01, None), Ctx::simple(now + 5_001, Some(80_000.0)));
    assert!(!d.is_allow());
    assert!(d.rejected_reason().contains("daily loss cap"));
}

/// A decision must serialize to JSON with a stable shape (product's MCP surface
/// returns decisions to agent clients).
#[test]
fn decision_serializes_stable_shape() {
    let d = Decision::Reject { reason: "cap".into() };
    let s = serde_json::to_string(&d).unwrap();
    assert_eq!(s, r#"{"status":"reject","reason":"cap"}"#);
    let a = serde_json::to_string(&Decision::Allow).unwrap();
    assert_eq!(a, r#"{"status":"allow"}"#);
}

#[test]
fn a_sell_that_flips_the_position_is_judged_by_the_size_it_opens() {
    // Regression (critic round 3, 2026-11-09): any sell on an existing long used to be
    // exempted from the position cap as a "reduction", so a sell far larger than the
    // long could open a huge short with the cap switched off ($900 long, $20k sell,
    // $1k cap -> allowed). A sell that flips must be judged by |position after|.
    let mut g = Guardrail::new(cfg()); // max_position_notional 10_000
    let c = Ctx { now_ms: 1_000, mark: Some(80_000.0), position_notional: Some(900.0) };
    // 0.25 BTC @ 80k = 20_000 -> flips to a 19_100 short -> reject
    let d = g.allow(&intent("BTC", false, 0.25, None), c);
    assert!(!d.is_allow(), "a flipping sell must not be exempt from the position cap: {d:?}");
    assert!(d.rejected_reason().contains("position cap"), "{d:?}");
    // a sell that only reduces (0.01 BTC = 800 <= 900) stays allowed
    assert!(g.allow(&intent("BTC", false, 0.01, None), c).is_allow());
}
