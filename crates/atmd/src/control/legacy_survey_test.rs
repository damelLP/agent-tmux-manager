//! Full viewport replies for legacy input tests using a tmux double.
use atm_tmux::MockTmuxClient;

pub(crate) fn fixture() -> String {
    include_str!(
        "../../../atm-claude-adapter/tests/fixtures/claude_feedback_survey_2.1.327.styled.txt"
    )
    .lines()
    .skip(1)
    .map(|line| format!("{}\n", line.strip_suffix('|').unwrap()))
    .collect()
}

pub(crate) fn viewport_stdout(pane: &str, text: &str, bracketed: Option<bool>) -> String {
    let flag = match bracketed {
        Some(true) => "1",
        Some(false) => "0",
        None => "",
    };
    let metadata = format!(
        "ATM-PANE-v2\t{pane}\tprivate\t0\t123\t217\t{}\t1\t0\t0\t{flag}\t0\t0\t\tterminal\t0\t0\n",
        text.lines().count()
    );
    format!(
        "{metadata}ATM-CAPTURE-v1\n{metadata}{text}ATM-CLIENTS-v1\nATM-END\t{pane}\nATM-TAG-v1\nATM-TAG-END-v1\n"
    )
}

pub(crate) fn mock_viewport(
    mock: &MockTmuxClient,
    pane: &str,
    text: &str,
    bracketed: Option<bool>,
) {
    mock.set_next_raw_output(&viewport_stdout(pane, text, bracketed));
}

pub(crate) fn mock_shell(mock: &MockTmuxClient, pane: &str) {
    mock_viewport(mock, pane, "shell$ \n", Some(true));
}
