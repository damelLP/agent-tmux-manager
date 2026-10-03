//! Property-based round-trip tests for the wire protocol.
//!
//! Invariant under test: for every `ClientMessage` / `DaemonMessage` we can
//! construct, `from_str(to_string(msg))` produces a value whose JSON
//! representation is identical to the original's. Comparing via
//! `serde_json::Value` lets us check structural equality without requiring
//! `PartialEq` on the protocol types (which contain `serde_json::Value` and
//! boxed `SessionView` payloads).

#![allow(clippy::unwrap_used, clippy::expect_used)] // test code may panic

use atm_core::{SessionId, SessionStatus, SessionView};
use atm_protocol::{ClientMessage, DaemonMessage, MessageType, ProtocolVersion};
use proptest::prelude::*;
use serde_json::Value;

// ---------------------------------------------------------------------------
// Round-trip helper
// ---------------------------------------------------------------------------

/// Serialize, then deserialize, then re-serialize. The original and
/// re-serialized JSON values must be structurally equal.
///
/// We compare via `serde_json::Value` instead of derived `PartialEq` because
/// the protocol types deliberately don't implement `PartialEq` (free-form
/// `Value` payloads, boxed views).
fn assert_json_roundtrip<T>(value: &T)
where
    T: serde::Serialize + serde::de::DeserializeOwned,
{
    let json = serde_json::to_string(value).expect("serialize original");
    let parsed: T = serde_json::from_str(&json).expect("deserialize round-trip");
    let reserialized = serde_json::to_string(&parsed).expect("serialize round-trip");

    let original_value: Value = serde_json::from_str(&json).expect("parse original as Value");
    let roundtrip_value: Value =
        serde_json::from_str(&reserialized).expect("parse round-trip as Value");

    prop_assert_json_eq(&original_value, &roundtrip_value);
}

#[track_caller]
fn prop_assert_json_eq(a: &Value, b: &Value) {
    assert_eq!(
        a, b,
        "round-trip mismatch:\n  original = {a}\n  roundtrip = {b}"
    );
}

// ---------------------------------------------------------------------------
// String / JSON strategies
// ---------------------------------------------------------------------------

/// Strategy producing strings that exercise edge cases we care about:
/// empty, ASCII, accented Latin, ZWSP, emoji, and large (~1 KiB) inputs.
fn arb_tricky_string() -> impl Strategy<Value = String> {
    prop_oneof![
        // Empty
        Just(String::new()),
        // Plain ASCII
        "[a-zA-Z0-9_./ -]{0,32}".prop_map(String::from),
        // Accents
        Just("café résumé naïve Ωμέγα".to_string()),
        // Zero-width space sandwich
        Just("a\u{200B}b\u{200B}c".to_string()),
        // Emoji + combining marks
        Just("hello 👋 world 🌍 e\u{0301}".to_string()),
        // Long string ~1 KiB (mix ASCII + multibyte to stress encoding)
        Just("x".repeat(1024)),
        Just("漢".repeat(256)),
        // Arbitrary unicode of moderate size
        proptest::collection::vec(any::<char>(), 0..64)
            .prop_map(|chars| chars.into_iter().collect::<String>()),
    ]
}

/// Bounded f64 strategy.
///
/// We deliberately stay away from subnormal / very-small-magnitude floats:
/// `serde_json`'s number formatting is not always 1-ULP exact for those
/// (an upstream limitation, not a protocol concern). Realistic protocol
/// f64s — cost in USD, context percentage, duration in seconds — sit
/// comfortably inside `±[1e-12, 1e12]`, so that's the protocol-meaningful
/// domain for round-trip checking.
///
/// Note that we have to bound the magnitude on *both* ends. A single
/// `-1e12..1e12` range still includes arbitrarily tiny magnitudes
/// (e.g. `1e-200`), which is exactly where serde_json's number formatting
/// drifts. Generating zero explicitly and requiring `|f| ∈ [1e-12, 1e12]`
/// keeps the test stable instead of flaky.
fn arb_realistic_f64() -> impl Strategy<Value = f64> {
    prop_oneof![
        Just(0.0_f64),
        Just(-0.0_f64),
        Just(1.0_f64),
        Just(-1.0_f64),
        // Positive magnitudes
        1.0e-12_f64..1.0e12_f64,
        // Negative magnitudes
        -1.0e12_f64..-1.0e-12_f64,
    ]
}

/// Strategy producing arbitrary JSON values with finite, realistic f64s.
///
/// `serde_json` cannot serialize NaN or Inf at all, and isn't ULP-exact for
/// extreme-magnitude finite f64s. Restricting numeric leaves up front keeps
/// failures from this test attributable to atm-protocol rather than
/// upstream JSON-number quirks.
fn arb_finite_json_value() -> impl Strategy<Value = Value> {
    let leaf = prop_oneof![
        Just(Value::Null),
        any::<bool>().prop_map(Value::Bool),
        any::<i64>().prop_map(|n| Value::Number(n.into())),
        any::<u64>().prop_map(|n| Value::Number(n.into())),
        arb_realistic_f64().prop_map(|f| {
            // `from_f64` only returns None for non-finite inputs, which our
            // bounded strategy never produces.
            serde_json::Number::from_f64(f)
                .map(Value::Number)
                .unwrap_or(Value::Null)
        }),
        arb_tricky_string().prop_map(Value::String),
    ];

    leaf.prop_recursive(
        4,  // depth
        32, // total nodes
        8,  // items per collection
        |inner| {
            prop_oneof![
                proptest::collection::vec(inner.clone(), 0..6).prop_map(Value::Array),
                proptest::collection::hash_map("[a-z]{1,8}", inner, 0..6)
                    .prop_map(|m| Value::Object(m.into_iter().collect())),
            ]
        },
    )
}

// ---------------------------------------------------------------------------
// Domain-specific strategies
// ---------------------------------------------------------------------------

fn arb_protocol_version() -> impl Strategy<Value = ProtocolVersion> {
    // Cover the full u16 range, including 0 and u16::MAX, on both fields.
    (any::<u16>(), any::<u16>()).prop_map(|(major, minor)| ProtocolVersion::new(major, minor))
}

fn arb_session_id() -> impl Strategy<Value = SessionId> {
    arb_tricky_string().prop_map(SessionId::new)
}

fn arb_session_status() -> impl Strategy<Value = SessionStatus> {
    prop_oneof![
        Just(SessionStatus::Idle),
        Just(SessionStatus::Working),
        Just(SessionStatus::AttentionNeeded),
    ]
}

/// Strategy for RFC3339-ish timestamp strings.
///
/// These fields are plain `String` on the wire (formatted upstream and
/// parsed downstream by callers, not by the protocol crate), so we mix
/// realistic shapes with the general tricky-string strategy. The point is
/// to confirm the protocol layer doesn't mangle whatever it's handed.
fn arb_timestamp_string() -> impl Strategy<Value = String> {
    prop_oneof![
        Just("1970-01-01T00:00:00+00:00".to_string()),
        Just("2026-05-08T12:34:56.789Z".to_string()),
        Just("9999-12-31T23:59:59-12:00".to_string()),
        arb_tricky_string(),
    ]
}

/// Builds a fully-populated `SessionView`.
///
/// Every field that appears on the wire is independently generated. We
/// derive `id_short`, `status_label`, `status_icon`, and `should_blink`
/// from `id`/`status` because those mirror what `SessionView::from_domain`
/// does in production — round-trip equality wouldn't be meaningful if we
/// fed the type inconsistent state.
///
/// Implementation notes:
/// - Proptest's tuple `Strategy` impl maxes out at 12 elements, so fields
///   are grouped into themed bundles.
/// - Each bundle is `.boxed()` to type-erase the intermediate `Strategy`.
///   Without boxing, the monomorphized generic type for the full chain
///   overflows the default 2 MiB test thread stack on debug builds (the
///   crash surfaces only when this function is composed into the larger
///   `arb_daemon_message` strategy via `SessionList`/`SessionUpdated`).
///
/// **If you add another field bundle here, also call `.boxed()` on it** —
/// otherwise the test binary may stack-overflow on `cargo test` without
/// any logical regression in the protocol itself.
fn arb_session_view() -> impl Strategy<Value = SessionView> {
    // NOTE: every bundle below ends in `.boxed()` on purpose — see the
    // doc comment above before removing or adding one.

    // Identity + status (4)
    let identity = (
        arb_session_id(),
        arb_tricky_string(), // agent_type
        arb_tricky_string(), // model
        arb_session_status(),
    )
        .boxed();

    // Numerics (3 f64s)
    let numerics = (
        arb_realistic_f64(),
        arb_realistic_f64(),
        arb_realistic_f64(),
    )
        .boxed();

    // Display strings (6)
    let displays = (
        arb_tricky_string(), // context_display
        arb_tricky_string(), // cost_display
        arb_tricky_string(), // duration_display
        arb_tricky_string(), // lines_display
        arb_tricky_string(), // last_activity_display
        arb_tricky_string(), // age_display
    )
        .boxed();

    // Independent booleans (3) — context_warning/critical/needs_attention
    // are not derived from the f64 in SessionView; they're carried verbatim
    // from the domain. Vary them independently to catch bool-field bugs.
    let bools = (any::<bool>(), any::<bool>(), any::<bool>()).boxed();

    // Optional strings (4)
    let opts = (
        proptest::option::of(arb_tricky_string()), // activity_detail
        proptest::option::of(arb_tricky_string()), // working_directory
        proptest::option::of(arb_tricky_string()), // tmux_pane
        proptest::option::of(arb_tricky_string()), // first_prompt
    )
        .boxed();

    // Git-related optionals (3)
    let git = (
        proptest::option::of(arb_tricky_string()), // project_root
        proptest::option::of(arb_tricky_string()), // worktree_path
        proptest::option::of(arb_tricky_string()), // worktree_branch
    )
        .boxed();

    // Session relations
    let relations = (
        proptest::option::of(arb_session_id()),
        proptest::collection::vec(arb_session_id(), 0..4),
    )
        .boxed();

    // Timestamps (2 strings)
    let timestamps = (arb_timestamp_string(), arb_timestamp_string()).boxed();

    (
        identity, numerics, displays, bools, opts, git, relations, timestamps,
    )
        .prop_map(
            |(
                (id, agent_type, model, status),
                (ctx_pct, cost_usd, duration_seconds),
                (
                    context_display,
                    cost_display,
                    duration_display,
                    lines_display,
                    last_activity_display,
                    age_display,
                ),
                (context_warning, context_critical, needs_attention),
                (activity_detail, working_directory, tmux_pane, first_prompt),
                (project_root, worktree_path, worktree_branch),
                (parent_session_id, child_session_ids),
                (started_at, last_activity),
            )| SessionView {
                id_short: id.short().to_string(),
                id,
                agent_type,
                harness: "claude".into(),
                model,
                status,
                status_label: status.label().to_string(),
                activity_detail,
                should_blink: status.should_blink(),
                status_icon: status.icon().to_string(),
                context_percentage: ctx_pct,
                context_display,
                context_warning,
                context_critical,
                cost_display,
                cost_usd,
                duration_display,
                duration_seconds,
                lines_display,
                working_directory,
                needs_attention,
                last_activity_display,
                age_display,
                started_at,
                last_activity,
                tmux_pane,
                project_root,
                worktree_path,
                worktree_branch,
                parent_session_id,
                child_session_ids,
                first_prompt,
            },
        )
}

// ---------------------------------------------------------------------------
// Client MessageType strategy (covers every variant)
// ---------------------------------------------------------------------------

fn arb_message_type() -> impl Strategy<Value = MessageType> {
    prop_oneof![
        // Connect { client_id: Option<String> }
        proptest::option::of(arb_tricky_string())
            .prop_map(|client_id| MessageType::Connect { client_id }),
        // StatusUpdate { data: Value }
        arb_finite_json_value().prop_map(|data| MessageType::StatusUpdate { data }),
        // HookEvent { data: Value }
        arb_finite_json_value().prop_map(|data| MessageType::HookEvent { data }),
        // ListSessions
        Just(MessageType::ListSessions),
        // Subscribe { session_id: Option<SessionId> }
        proptest::option::of(arb_session_id())
            .prop_map(|session_id| MessageType::Subscribe { session_id }),
        // Unsubscribe
        Just(MessageType::Unsubscribe),
        // Ping { seq: u64 } — include u64::MAX
        prop_oneof![Just(0u64), Just(u64::MAX), any::<u64>()]
            .prop_map(|seq| MessageType::Ping { seq }),
        // Disconnect
        Just(MessageType::Disconnect),
        // Discover
        Just(MessageType::Discover),
    ]
}

fn arb_client_message() -> impl Strategy<Value = ClientMessage> {
    (arb_protocol_version(), arb_message_type()).prop_map(|(protocol_version, message)| {
        ClientMessage {
            protocol_version,
            message,
        }
    })
}

// ---------------------------------------------------------------------------
// DaemonMessage strategy (covers every variant)
// ---------------------------------------------------------------------------

fn arb_daemon_message() -> impl Strategy<Value = DaemonMessage> {
    prop_oneof![
        // Connected
        (arb_protocol_version(), arb_tricky_string()).prop_map(|(protocol_version, client_id)| {
            DaemonMessage::Connected {
                protocol_version,
                client_id,
            }
        }),
        // Rejected
        (arb_tricky_string(), arb_protocol_version()).prop_map(|(reason, protocol_version)| {
            DaemonMessage::Rejected {
                reason,
                protocol_version,
            }
        }),
        // SessionList { sessions }
        proptest::collection::vec(arb_session_view(), 0..4)
            .prop_map(|sessions| DaemonMessage::SessionList { sessions }),
        // SessionUpdated { session: Box<SessionView> }
        arb_session_view().prop_map(|session| DaemonMessage::SessionUpdated {
            session: Box::new(session),
        }),
        // SessionRemoved { session_id }
        arb_session_id().prop_map(|session_id| DaemonMessage::SessionRemoved { session_id }),
        // Pong { seq } — include u64::MAX
        prop_oneof![Just(0u64), Just(u64::MAX), any::<u64>()]
            .prop_map(|seq| DaemonMessage::Pong { seq }),
        // Error { message, code: Option<String> }
        (
            arb_tricky_string(),
            proptest::option::of(arb_tricky_string())
        )
            .prop_map(|(message, code)| DaemonMessage::Error { message, code }),
        // DiscoveryComplete { discovered, failed }
        (any::<u32>(), any::<u32>()).prop_map(|(discovered, failed)| {
            DaemonMessage::DiscoveryComplete { discovered, failed }
        }),
    ]
}

// ---------------------------------------------------------------------------
// Properties
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig {
        cases: 256,
        .. ProptestConfig::default()
    })]

    #[test]
    fn protocol_version_roundtrip(v in arb_protocol_version()) {
        assert_json_roundtrip(&v);
    }

    #[test]
    fn message_type_roundtrip(m in arb_message_type()) {
        assert_json_roundtrip(&m);
    }

    #[test]
    fn client_message_roundtrip(m in arb_client_message()) {
        assert_json_roundtrip(&m);
    }

    #[test]
    fn daemon_message_roundtrip(m in arb_daemon_message()) {
        assert_json_roundtrip(&m);
    }

    #[test]
    fn session_view_roundtrip(v in arb_session_view()) {
        assert_json_roundtrip(&v);
    }
}

// ---------------------------------------------------------------------------
// Targeted edge cases — these are concrete values worth pinning even if the
// random generator already covers them, because failures here are easier to
// read than a shrunk proptest counterexample.
// ---------------------------------------------------------------------------

#[test]
fn ping_u64_max_roundtrip() {
    let msg = ClientMessage::ping(u64::MAX);
    assert_json_roundtrip(&msg);
}

#[test]
fn pong_u64_max_roundtrip() {
    let msg = DaemonMessage::pong(u64::MAX);
    assert_json_roundtrip(&msg);
}

#[test]
fn protocol_version_u16_max_roundtrip() {
    let v = ProtocolVersion::new(u16::MAX, u16::MAX);
    assert_json_roundtrip(&v);
}

#[test]
fn status_update_with_nested_json_roundtrip() {
    let data = serde_json::json!({
        "session_id": "8e11bfb5-7dc2-432b-9206-928fa5c35731",
        "model": { "id": "claude-opus-4-7", "display_name": "Opus 4.7" },
        "cost": { "total_cost_usd": 1.23, "total_duration_ms": 5000u64 },
        "unicode": "café 👋 \u{200B} 漢字"
    });
    let msg = ClientMessage::status_update(data);
    assert_json_roundtrip(&msg);
}
