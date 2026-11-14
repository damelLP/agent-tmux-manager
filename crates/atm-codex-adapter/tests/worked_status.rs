use atm_codex_adapter::screen::{observe_control_screen, observe_screen};
use atm_core::{ScreenInput, TurnState};

macro_rules! styled {
    ($name:literal) => {
        include_str!(concat!("fixtures/screen_styled_p1_", $name, "_0.167.0.txt"))
            .split_once('\n')
            .unwrap()
            .1
    };
}

const IDLE: &str = include_str!("fixtures/screen_codex-after-clear_0.167.0.txt");
const WORKED: &str = "Worked for 1s • 11:31 AM";

#[test]
fn captured_finished_turns_and_historical_worked_lines() {
    // Captured: codex-cli 0.167.0, 2026-11-14, U5 and P1 private tmux;
    // fixture README records capture flags and sanitization. An old Worked line
    // cannot override a running turn or make an unsupported dialog idle.
    for (text, turn) in [
        (styled!("c02-multiline-100x24"), TurnState::NotRunning),
        (styled!("c03-draft-before-100x24"), TurnState::NotRunning),
        (
            styled!("c11-working-released-100x24"),
            TurnState::NotRunning,
        ),
        (
            styled!("c18-permission-enter-approved-100x32"),
            TurnState::NotRunning,
        ),
        (
            styled!("c21-permission-multi-approved-100x32"),
            TurnState::NotRunning,
        ),
        (styled!("c07-working-held-100x24"), TurnState::Running),
        (
            styled!("b31-nodaemon-clear-dialog-100x32"),
            TurnState::Unknown,
        ),
        (styled!("c16-permission-visible-100x32"), TurnState::Unknown),
    ] {
        assert_eq!(observe_screen(ScreenInput::Styled(text), None).turn, turn);
        assert_eq!(
            observe_control_screen(ScreenInput::Styled(text), None)
                .facts
                .turn,
            turn
        );
    }
}

#[test]
fn finished_turn_clock_boundaries_and_independent_duration_shapes() {
    // Synthetic: vary captured seconds/minutes and both clock grammars independently.
    // Hour shapes follow the codex-cli 0.167.0 binary formatter; not seen on screen.
    for elapsed in [
        "<1s",
        "1s",
        "12s",
        "59s",
        "1m 0s",
        "1m 5s",
        "12m 3s",
        "59m 59s",
        "1h 0m 0s",
        "1h 2m 30s",
        "12h 59m 59s",
    ] {
        for clock in [
            "00:00", "09:05", "12:00", "23:59", "1:00 AM", "9:45 PM", "12:59 AM", "12:00 PM",
        ] {
            let line = format!("Worked for {elapsed} • {clock}");
            let text = IDLE.replace(WORKED, &line);
            assert_eq!(
                observe_screen(ScreenInput::Plain(&text), None).turn,
                TurnState::NotRunning,
                "{line}"
            );
        }
    }
}

#[test]
fn unsupported_duration_and_invalid_clock_mutations_stay_unknown() {
    // Synthetic: unsupported units, signs, fractions, ordering and whitespace
    // must not authorize NotRunning merely by resembling a Worked line.
    for elapsed in [
        "",
        "<0s",
        "<2s",
        "<1",
        "1",
        "1ms",
        "1m",
        "1h",
        "0s",
        "01s",
        "60s",
        "999s",
        "1m 05s",
        "0m 5s",
        "1m 60s",
        "60m 0s",
        "01m 5s",
        "1m5s",
        "1h 5s",
        "0h 2m 30s",
        "01h 2m 30s",
        "1h 02m 30s",
        "1h 2m 03s",
        "1h 60m 0s",
        "1h 0m 60s",
        "1h  2m 30s",
        "1h 2m 30s 1s",
        "18446744073709551616h 0m 0s",
        "1s 2s",
        "-1s",
        "+1s",
        "1.5s",
        "１s",
        "s",
        " 1s",
        "1s ",
    ] {
        for clock in ["16:34", "11:31 AM"] {
            assert_unknown(&format!("Worked for {elapsed} • {clock}"));
        }
    }
    for clock in [
        "",
        "25:61",
        "24:00",
        "23:60",
        "00:00 AM",
        "09:45 PM",
        "01:00 AM",
        "Nov 13 at 16:34",
        "Dec 31, 2025 at 11:59 PM",
        "13:00 PM",
        "12:60 PM",
        "1:00",
        "001:00 AM",
        "016:34",
        "16:3",
        "16:034",
        "+1:34",
        "+1:34 AM",
        "-1:34",
        "１:34 AM",
        "16:３４",
        "16:34:00",
        "16:34 PM",
        "11:31 am",
        "11:31 AM extra",
        "11:31  AM",
        " 16:34",
        "16:34 UTC",
    ] {
        assert_unknown(&format!("Worked for 1s • {clock}"));
    }
    for line in [
        "Not Worked for 1s • 16:34",
        "Worked for 1s · 16:34",
        "Worked for 1s•16:34",
        "Worked for 1s • 16:34 extra",
        "Worked for 1s • 16:34 • 16:35",
        "Worked for 1s • 16:34\nUnrecognized activity",
    ] {
        assert_unknown(line);
    }
}

fn assert_unknown(line: &str) {
    let text = IDLE.replace(WORKED, line);
    assert_ne!(text, IDLE, "mutation must change the capture");
    assert_eq!(
        observe_screen(ScreenInput::Plain(&text), None).turn,
        TurnState::Unknown,
        "{line:?}"
    );
    // Synthetic: also mutate every historical completion in the real styled
    // c02 frame so an older valid line cannot mask a malformed final one.
    let text = styled!("c02-multiline-100x24").replace("Worked for <1s • 16:34", line);
    assert_eq!(
        observe_screen(ScreenInput::Styled(&text), None).turn,
        TurnState::Unknown,
        "{line:?}"
    );
}
