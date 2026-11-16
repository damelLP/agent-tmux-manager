#![allow(clippy::unwrap_used, clippy::expect_used)] // fixed test dates
//! The handover policy, one test per rule of `atm_org::idle`'s table, with the
//! edges on both sides of every limit. Time is always an argument.

use atm_org::idle::{
    handover_step, CacheLimits, HandoverInput, HandoverLimits, HandoverReason, HandoverStep,
    WaitsFor,
};
use chrono::{DateTime, TimeDelta, TimeZone, Utc};
use proptest::prelude::*;
use std::time::Duration;

const MINUTE: i64 = 60;
const LIFETIME: i64 = 60 * MINUTE;
const IDLE_LIMIT: i64 = 50 * MINUTE;

/// The turn end every test measures from.
fn turn_end() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 11, 15, 12, 0, 0).unwrap()
}

/// `secs` after the turn end (before it when negative).
fn after(secs: i64) -> DateTime<Utc> {
    turn_end() + TimeDelta::seconds(secs)
}

fn cache(lifetime: i64, idle_limit: i64) -> Option<CacheLimits> {
    Some(CacheLimits {
        lifetime: Duration::from_secs(lifetime.unsigned_abs()),
        idle_limit: Duration::from_secs(idle_limit.unsigned_abs()),
    })
}

/// Line 40, a one-hour cache, an idle limit of fifty minutes.
fn limits() -> HandoverLimits {
    HandoverLimits {
        context_line_percent: 40,
        cache: cache(LIFETIME, IDLE_LIMIT),
    }
}

/// An idle agent at `share` percent that waits for another agent's work (which
/// comes in time), the owner present, the role's automatic switch on.
fn idle(share: f64) -> HandoverInput {
    HandoverInput {
        context_percent: Some(share),
        idle_since: Some(turn_end()),
        waits_for: WaitsFor::Agent,
        away: false,
        automatic: true,
    }
}

fn waiting(waits_for: WaitsFor) -> HandoverInput {
    HandoverInput {
        waits_for,
        ..idle(10.0)
    }
}

fn step(input: HandoverInput, secs_idle: i64) -> HandoverStep {
    handover_step(&input, &limits(), after(secs_idle))
}

const FLAG_LINE: HandoverStep = HandoverStep::Flag(HandoverReason::ContextLine);
const SAVE_LINE: HandoverStep = HandoverStep::Save(HandoverReason::ContextLine);
const SAVE_EXPIRING: HandoverStep = HandoverStep::Save(HandoverReason::CacheExpiring);
const CLEAR_COLD: HandoverStep = HandoverStep::Clear(HandoverReason::CacheCold);

// Rule 1: every act is at a turn end.

#[test]
fn a_running_turn_gets_nothing_whatever_else_holds() {
    let input = HandoverInput {
        idle_since: None,
        waits_for: WaitsFor::Nothing,
        away: true,
        ..idle(99.0)
    };
    assert_eq!(step(input, 10 * LIFETIME), HandoverStep::Nothing);
}

// Rule 2: a cold session is cleared, with no save turn.

#[test]
fn idle_for_exactly_the_cache_lifetime_is_cold() {
    assert_eq!(step(idle(10.0), LIFETIME), CLEAR_COLD);
    assert_eq!(step(idle(10.0), LIFETIME - 1), HandoverStep::Nothing);
}

#[test]
fn cold_beats_the_context_line() {
    assert_eq!(step(idle(90.0), LIFETIME), CLEAR_COLD);
    assert_eq!(step(idle(90.0), LIFETIME - 1), SAVE_LINE);
}

#[test]
fn a_cold_session_is_only_flagged_when_automatic_is_off() {
    let input = HandoverInput {
        automatic: false,
        ..idle(10.0)
    };
    assert_eq!(
        step(input, LIFETIME),
        HandoverStep::Flag(HandoverReason::CacheCold)
    );
}

#[test]
fn the_cache_lifetime_is_data() {
    let five_minutes = HandoverLimits {
        cache: cache(5 * MINUTE, 4 * MINUTE),
        ..limits()
    };
    let now = after(6 * MINUTE);
    assert_eq!(handover_step(&idle(10.0), &five_minutes, now), CLEAR_COLD);
    assert_eq!(
        handover_step(&idle(10.0), &limits(), now),
        HandoverStep::Nothing
    );
}

// Rule 3: the context line, the owner's main reason.

#[test]
fn a_share_exactly_at_the_line_is_flagged() {
    let at = |share| HandoverInput {
        automatic: false,
        ..idle(share)
    };
    assert_eq!(step(at(40.0), MINUTE), FLAG_LINE);
    assert_eq!(step(at(39.99), MINUTE), HandoverStep::Nothing);
}

#[test]
fn the_context_line_is_data() {
    let seventy = HandoverLimits {
        context_line_percent: 70,
        ..limits()
    };
    let at = |share| HandoverInput {
        automatic: false,
        ..idle(share)
    };
    let now = after(MINUTE);
    assert_eq!(
        handover_step(&at(69.99), &seventy, now),
        HandoverStep::Nothing
    );
    assert_eq!(handover_step(&at(70.0), &seventy, now), FLAG_LINE);
}

#[test]
fn at_the_line_an_automatic_role_is_asked_to_save() {
    assert_eq!(step(idle(40.0), MINUTE), SAVE_LINE);
    assert_eq!(step(idle(39.99), MINUTE), HandoverStep::Nothing);
}

#[test]
fn the_line_answers_at_the_turn_end_itself() {
    // H2a's call: at the turn end, automatic off.
    let input = HandoverInput {
        automatic: false,
        ..idle(55.5)
    };
    assert_eq!(step(input, 0), FLAG_LINE);
}

#[test]
fn an_unknown_share_never_reaches_the_line() {
    for context_percent in [None, Some(f64::NAN)] {
        let input = HandoverInput {
            context_percent,
            ..idle(0.0)
        };
        assert_eq!(step(input, MINUTE), HandoverStep::Nothing);
    }
}

#[test]
fn the_default_limits_are_the_sixty_percent_line_and_no_cache_numbers() {
    let default = HandoverLimits::default();
    assert_eq!(
        default,
        HandoverLimits {
            context_line_percent: 60,
            cache: None
        }
    );
    // With no cache numbers the idle rules are off, however long it idles.
    let a_day = after(24 * 60 * MINUTE);
    assert_eq!(
        handover_step(&waiting(WaitsFor::Nothing), &default, a_day),
        HandoverStep::Nothing
    );
    assert_eq!(handover_step(&idle(60.0), &default, a_day), SAVE_LINE);
    assert_eq!(
        handover_step(&idle(59.99), &default, a_day),
        HandoverStep::Nothing
    );
}

// Rule 4: idle past the idle limit, and what it waits for will not come
// before the cache expires.

#[test]
fn idle_for_exactly_the_idle_limit_with_nothing_to_wait_for_is_asked_to_save() {
    let input = waiting(WaitsFor::Nothing);
    assert_eq!(step(input, IDLE_LIMIT), SAVE_EXPIRING);
    assert_eq!(step(input, IDLE_LIMIT - 1), HandoverStep::Nothing);
}

#[test]
fn the_owner_answers_in_time_unless_he_is_away() {
    let here = waiting(WaitsFor::Owner);
    let away = HandoverInput { away: true, ..here };
    assert_eq!(step(here, IDLE_LIMIT), HandoverStep::Nothing);
    assert_eq!(step(away, IDLE_LIMIT), SAVE_EXPIRING);
    assert_eq!(step(away, IDLE_LIMIT - 1), HandoverStep::Nothing);
}

#[test]
fn away_changes_nothing_for_an_agent_that_does_not_wait_for_the_owner() {
    let expiry = after(LIFETIME);
    for (waits_for, expected) in [
        (WaitsFor::Agent, HandoverStep::Nothing),
        (WaitsFor::Nothing, SAVE_EXPIRING),
        (WaitsFor::Until(expiry), SAVE_EXPIRING),
        (
            WaitsFor::Until(expiry - TimeDelta::seconds(1)),
            HandoverStep::Nothing,
        ),
    ] {
        let here = waiting(waits_for);
        let away = HandoverInput { away: true, ..here };
        for input in [here, away] {
            assert_eq!(step(input, IDLE_LIMIT), expected, "{input:?}");
            assert_eq!(step(input, IDLE_LIMIT - 1), HandoverStep::Nothing);
        }
    }
    let agent_away = HandoverInput {
        away: true,
        ..waiting(WaitsFor::Agent)
    };
    assert_eq!(step(agent_away, LIFETIME - 1), HandoverStep::Nothing);
}

#[test]
fn a_known_time_comes_in_time_only_before_the_cache_expires() {
    let expiry = after(LIFETIME);
    let before = waiting(WaitsFor::Until(expiry - TimeDelta::seconds(1)));
    let at = waiting(WaitsFor::Until(expiry));
    assert_eq!(step(before, IDLE_LIMIT), HandoverStep::Nothing);
    assert_eq!(step(at, IDLE_LIMIT), SAVE_EXPIRING);
}

#[test]
fn a_time_that_is_already_due_comes_in_time() {
    let overdue = waiting(WaitsFor::Until(after(-MINUTE)));
    assert_eq!(step(overdue, IDLE_LIMIT), HandoverStep::Nothing);
}

#[test]
fn an_expiring_cache_is_only_flagged_when_automatic_is_off() {
    let input = HandoverInput {
        automatic: false,
        ..waiting(WaitsFor::Nothing)
    };
    assert_eq!(
        step(input, IDLE_LIMIT),
        HandoverStep::Flag(HandoverReason::CacheExpiring)
    );
}

#[test]
fn the_line_beats_the_idle_limit() {
    let input = HandoverInput {
        waits_for: WaitsFor::Nothing,
        ..idle(40.0)
    };
    assert_eq!(step(input, IDLE_LIMIT), SAVE_LINE);
}

// Rule 5.

#[test]
fn an_idle_agent_below_every_limit_gets_nothing() {
    let input = HandoverInput {
        away: true,
        ..waiting(WaitsFor::Nothing)
    };
    assert_eq!(step(input, 0), HandoverStep::Nothing);
    assert_eq!(step(input, 10 * MINUTE), HandoverStep::Nothing);
}

// Edges.

#[test]
fn a_clock_that_stepped_back_counts_as_no_idle_time() {
    assert_eq!(
        step(waiting(WaitsFor::Nothing), -2 * LIFETIME),
        HandoverStep::Nothing
    );
    assert_eq!(step(idle(40.0), -2 * LIFETIME), SAVE_LINE);
}

#[test]
fn the_largest_durations_and_dates_do_not_overflow() {
    let forever = HandoverLimits {
        cache: Some(CacheLimits {
            lifetime: Duration::MAX,
            idle_limit: Duration::ZERO,
        }),
        ..limits()
    };
    let input = HandoverInput {
        idle_since: Some(DateTime::<Utc>::MIN_UTC),
        ..waiting(WaitsFor::Until(DateTime::<Utc>::MAX_UTC))
    };
    assert_eq!(
        handover_step(&input, &forever, DateTime::<Utc>::MAX_UTC),
        HandoverStep::Nothing
    );
}

#[test]
fn an_idle_limit_at_or_above_the_lifetime_never_asks_for_a_save() {
    let wrong = HandoverLimits {
        cache: cache(LIFETIME, LIFETIME),
        ..limits()
    };
    let input = waiting(WaitsFor::Nothing);
    assert_eq!(
        handover_step(&input, &wrong, after(LIFETIME - 1)),
        HandoverStep::Nothing
    );
    assert_eq!(handover_step(&input, &wrong, after(LIFETIME)), CLEAR_COLD);
}

// Properties over the whole input space.

// Half of the times lie within two days of the turn end and half of the
// durations are under two days, so that idle times and limits meet and every
// row of the table is reached; the other halves span the whole range.
const NEAR: i64 = 2 * 24 * 60 * MINUTE;

fn any_time() -> impl Strategy<Value = DateTime<Utc>> {
    let range = DateTime::<Utc>::MIN_UTC.timestamp()..=DateTime::<Utc>::MAX_UTC.timestamp();
    prop_oneof![
        (-NEAR..=NEAR).prop_map(after),
        (range, 0u32..1_000_000_000).prop_map(|(secs, nanos)| {
            DateTime::from_timestamp(secs, nanos).unwrap_or(DateTime::<Utc>::MAX_UTC)
        }),
    ]
}

fn any_duration() -> impl Strategy<Value = Duration> {
    prop_oneof![
        (0..=NEAR.unsigned_abs()).prop_map(Duration::from_secs),
        (any::<u64>(), 0u32..1_000_000_000).prop_map(|(secs, nanos)| Duration::new(secs, nanos)),
    ]
}

fn any_waits_for() -> impl Strategy<Value = WaitsFor> {
    prop_oneof![
        Just(WaitsFor::Nothing),
        Just(WaitsFor::Owner),
        Just(WaitsFor::Agent),
        any_time().prop_map(WaitsFor::Until),
    ]
}

fn any_input() -> impl Strategy<Value = HandoverInput> {
    (
        proptest::option::of(any::<f64>()),
        proptest::option::of(any_time()),
        any_waits_for(),
        any::<bool>(),
        any::<bool>(),
    )
        .prop_map(
            |(context_percent, idle_since, waits_for, away, automatic)| HandoverInput {
                context_percent,
                idle_since,
                waits_for,
                away,
                automatic,
            },
        )
}

fn any_limits() -> impl Strategy<Value = HandoverLimits> {
    let cache = (any_duration(), any_duration()).prop_map(|(lifetime, idle_limit)| CacheLimits {
        lifetime,
        idle_limit,
    });
    (any::<u8>(), proptest::option::of(cache)).prop_map(|(context_line_percent, cache)| {
        HandoverLimits {
            context_line_percent,
            cache,
        }
    })
}

proptest! {
    #[test]
    fn any_input_gets_an_answer_and_only_an_idle_automatic_agent_is_acted_on(
        input in any_input(),
        limits in any_limits(),
        now in any_time(),
    ) {
        let step = handover_step(&input, &limits, now);
        if input.idle_since.is_none() {
            prop_assert_eq!(step, HandoverStep::Nothing);
        }
        if !input.automatic {
            prop_assert!(matches!(step, HandoverStep::Nothing | HandoverStep::Flag(_)));
        }
        // A clear is never asked for a reason a save turn could still serve.
        if let HandoverStep::Clear(reason) = step {
            prop_assert_eq!(reason, HandoverReason::CacheCold);
        }
        if let HandoverStep::Save(reason) = step {
            prop_assert_ne!(reason, HandoverReason::CacheCold);
        }
    }
}
