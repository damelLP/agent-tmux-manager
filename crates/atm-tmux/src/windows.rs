//! Pane/window observations from one tmux command, including repeated linked windows.
//!
//! Captured: tmux 3.8 and 3.6a (package 3.6a-2ubuntu0.2), 2026-11-17,
//! private `list-panes -a -F`; full captures are in `tests/fixtures`.
//! Linked/grouped windows repeat once per session. 3.8 rejects control-bearing
//! names; 3.6a stores printable backslash escapes instead. Automatic names may be empty.

use crate::TmuxError;

pub(crate) const FORMAT: &str =
    "#{pane_id}\t#{window_id}\t#{session_name}\t#{window_index}\t#{window_name}";

/// One pane's window and one of that window's session links.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PaneWindow {
    pub pane_id: String,
    pub window_id: String,
    pub session_name: String,
    pub window_index: u32,
    pub window_name: String,
}

fn numeric_id(value: &str, prefix: char) -> bool {
    value.strip_prefix(prefix).is_some_and(|number| {
        !number.is_empty()
            && number.bytes().all(|b| b.is_ascii_digit())
            && number.parse::<u64>().is_ok()
    })
}

/// Parse all rows or fail, so corrupt output cannot replace a good snapshot with a partial one.
pub fn parse_pane_windows(output: &str) -> Result<Vec<PaneWindow>, TmuxError> {
    output
        .lines()
        .filter(|line| !line.is_empty())
        .enumerate()
        .map(|(index, line)| {
            let bad = || TmuxError::ParseError(format!("invalid pane/window row {}", index + 1));
            let mut fields = line.splitn(5, '\t');
            let (Some(pane), Some(window), Some(session), Some(index), Some(name)) = (
                fields.next(),
                fields.next(),
                fields.next(),
                fields.next(),
                fields.next(),
            ) else {
                return Err(bad());
            };
            if !numeric_id(pane, '%')
                || !numeric_id(window, '@')
                || session.is_empty()
                || session.chars().any(char::is_control)
                || index.is_empty()
                || !index.bytes().all(|b| b.is_ascii_digit())
            {
                return Err(bad());
            }
            Ok(PaneWindow {
                pane_id: pane.into(),
                window_id: window.into(),
                session_name: session.into(),
                window_index: index.parse().map_err(|_| bad())?,
                window_name: name.into(),
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{mock::MockCall, MockTmuxClient, TmuxClient, TmuxError};

    // Captured: tmux 3.8 and 3.6a (package 3.6a-2ubuntu0.2), 2026-11-17,
    // private list-panes -a -F; grouped_linked in tests/fixtures/*_pane_windows.json.
    const GROUPED: &str = "%0\t@0\talpha\t0\tsupport\n%0\t@0\tbeta\t0\tsupport\n%2\t@2\tgamma\t0\tsh\n%0\t@0\tgamma\t4\tsupport\n";

    // Break caught: per-pane queries, loss of links, or a format without window id/name.
    #[tokio::test]
    async fn windows_default_method_reads_one_all_panes_capture() {
        let mock = MockTmuxClient::new();
        mock.set_next_raw_output(GROUPED);
        let client: &dyn TmuxClient = &mock;
        let rows = client.list_pane_windows().await.unwrap();
        assert_eq!(rows.len(), 4);
        assert_eq!(
            rows[0],
            PaneWindow {
                pane_id: "%0".into(),
                window_id: "@0".into(),
                session_name: "alpha".into(),
                window_index: 0,
                window_name: "support".into(),
            }
        );
        assert_eq!(rows[3].session_name, "gamma");
        assert_eq!(rows[3].window_index, 4);
        assert_eq!(
            mock.calls(),
            vec![MockCall::RawCommand {
                subcommand: "list-panes".into(),
                args: vec![
                    "-a".into(),
                    "-F".into(),
                    "#{pane_id}\t#{window_id}\t#{session_name}\t#{window_index}\t#{window_name}"
                        .into()
                ],
            }]
        );
    }

    // Break caught: trimming fields or assuming automatic-rename always gives a name.
    #[test]
    fn windows_parser_keeps_unicode_escaped_and_empty_names() {
        // Captured: tmux 3.8, 2026-11-17, unicode_name / automatic_observation.
        let rows =
            parse_pane_windows("%0\t@0\talpha\t0\t支援 café 🚀\n%4\t@5\talpha\t1\t\n").unwrap();
        assert_eq!(rows[0].window_name, "支援 café 🚀");
        assert_eq!(rows[1].window_name, "");
        // Captured: tmux 3.6a, 2026-11-17, control_0 / control_3.
        let rows = parse_pane_windows("%0\t@0\talpha\t0\ttab\\there\n%2\t@2\ttab\\there\t0\tsh\n")
            .unwrap();
        assert_eq!(rows[0].window_name, "tab\\there");
        assert_eq!(rows[1].session_name, "tab\\there");
        assert!(parse_pane_windows("").unwrap().is_empty());
    }

    // Synthetic: corrupt a captured row; accepting just its valid prefix would erase good data.
    #[test]
    fn windows_parser_refuses_malformed_nonempty_rows_without_partial_success() {
        for bad in [
            "junk",
            "%1\t@0\talpha\t0",
            "1\t@0\talpha\t0\tx",
            "%1\t0\talpha\t0\tx",
            "%\t@0\talpha\t0\tx",
            "%1\t@0\t\t0\tx",
            "%1\t@0\talpha\t-1\tx",
            "%1\t@0\talpha\t+1\tx",
            "%1\t@0\talpha\t4294967296\tx",
            "%1\t@0\talpha\x1b\t0\tx",
        ] {
            assert!(
                matches!(
                    parse_pane_windows(&format!("{GROUPED}{bad}\n")),
                    Err(TmuxError::ParseError(_))
                ),
                "{bad:?}"
            );
        }
    }

    // Break caught: swallowing unavailable/timed-out tmux as an available empty observation.
    #[tokio::test]
    async fn windows_default_method_propagates_tmux_failure() {
        let mock = MockTmuxClient::new();
        mock.set_next_error(TmuxError::NotFound);
        assert!(matches!(
            mock.list_pane_windows().await,
            Err(TmuxError::NotFound)
        ));
    }
}
