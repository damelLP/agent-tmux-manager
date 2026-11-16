use super::{agent_launch::launch_command, agent_project::PreparedAgent, agents::AgentError};
use atm_core::Harness;

fn prepared(harness: &str) -> PreparedAgent {
    PreparedAgent {
        agent_id: "project/lead".parse().unwrap(),
        root: "/work".into(),
        role: atm_org::parse_role("lead", &format!("+++\nharness = '{harness}'\n+++\n")).unwrap(),
        cwd: "/work".into(),
        trust_dirs: vec![],
    }
}

fn refused(result: Result<super::agent_launch::LaunchCommand, AgentError>, reason: &str) {
    assert!(
        matches!(result, Err(AgentError::Refused(ref text)) if text.contains(reason)),
        "{result:?}"
    );
}

#[test]
fn up_launch_command_quoting() {
    let mut agent = prepared("pi");
    agent.cwd = "/work/it's here".into();
    agent.role.config.command_prefix = vec!["/bin/wrap per".into(), "it's $literal".into()];
    agent.role.config.spawn_args = vec!["--profile".into(), "a' b;$x".into(), "".into()];
    agent.role.config.model = Some("model ' $x".into());
    let command = launch_command(&agent, |_| None).unwrap();
    assert_eq!(command.text, "cd '/work/it'\\''s here' && ATM_AGENT='project/lead' '/bin/wrap per' 'it'\\''s $literal' pi '--profile' 'a'\\'' b;$x' '' --model 'model '\\'' $x'");
    assert_eq!(command.binary, "pi");
}

#[test]
fn up_binary_override_keeps_required_and_role_args() {
    let mut agent = prepared("codex");
    agent.role.config.spawn_args = vec!["--no-daemon".into(), "--profile".into(), "a b".into()];
    agent.role.config.model = Some("model name".into());
    agent.role.config.command_prefix = vec!["wrapper".into()];
    let command = launch_command(&agent, |key| match key {
        "ATM_SPAWN_BIN" => Some("/bin/test agent".into()),
        "ATM_SPAWN_ARGS" => Some("  --raw value  ".into()),
        _ => None,
    })
    .unwrap();
    assert_eq!(command.text, "cd '/work' && ATM_AGENT='project/lead' 'wrapper' '/bin/test agent' --raw value '--no-daemon' '--profile' 'a b' -m 'model name'");
    assert_eq!(command.binary, "/bin/test agent");
}

#[test]
fn harness_specific_override_wins() {
    for (specific_bin, global_bin, specific_args, global_args, binary, suffix) in [
        (
            Some("specific"),
            Some("global"),
            Some("--specific"),
            Some("--global"),
            "specific",
            "specific --specific",
        ),
        (
            None,
            Some("global"),
            None,
            Some("--global"),
            "global",
            "global --global",
        ),
        (
            Some(""),
            Some("global"),
            Some(""),
            Some("--global"),
            "global",
            "global --global",
        ),
        (Some(""), Some(""), Some(""), Some(""), "claude", "claude"),
        (
            Some("specific"),
            Some("global"),
            None,
            Some("--global"),
            "specific",
            "specific --global",
        ),
        (
            None,
            Some("global"),
            Some("--specific"),
            Some("--global"),
            "global",
            "global --specific",
        ),
        (
            Some(" "),
            Some("global"),
            Some(" "),
            Some("--global"),
            " ",
            "' ' ",
        ),
    ] {
        // Role aliases must resolve to the canonical CLAUDE override key.
        let command = launch_command(&prepared("cc"), |key| match key {
            "ATM_SPAWN_CLAUDE_BIN" => specific_bin.map(str::to_owned),
            "ATM_SPAWN_BIN" => global_bin.map(str::to_owned),
            "ATM_SPAWN_CLAUDE_ARGS" => specific_args.map(str::to_owned),
            "ATM_SPAWN_ARGS" => global_args.map(str::to_owned),
            _ => Some("wrong-harness".into()),
        })
        .unwrap();
        assert_eq!(command.binary, binary);
        assert_eq!(
            command.text,
            format!("cd '/work' && ATM_AGENT='project/lead' {suffix}")
        );
    }
}

#[test]
fn up_ignores_harness_config_table() {
    // CLI config is deliberately not an input. A config-path lookup through the
    // injected environment would violate this boundary; no process env mutation.
    let command = launch_command(&prepared("codex"), |key| match key {
        "ATM_SPAWN_CODEX_BIN" | "ATM_SPAWN_BIN" | "ATM_SPAWN_CODEX_ARGS" | "ATM_SPAWN_ARGS" => None,
        _ => panic!("unexpected config/environment lookup: {key}"),
    })
    .unwrap();
    assert_eq!(
        command.text,
        "cd '/work' && ATM_AGENT='project/lead' codex '--no-daemon'"
    );
    assert_eq!(command.binary, "codex");
}

#[test]
fn up_non_ascii_launch_text_refuses() {
    for value in [
        "a界b", "a\nb", "a\tb", "a\0b", "a\u{7f}b", "a\rb", "a\u{1b}b", "a b",
    ] {
        for field in [
            "cwd",
            "model",
            "args",
            "prefix",
            "id",
            "bin_override",
            "args_override",
        ] {
            let mut agent = prepared("pi");
            match field {
                "cwd" => agent.cwd = format!("/work/{value}").into(),
                "model" => agent.role.config.model = Some(value.into()),
                "args" => agent.role.config.spawn_args = vec![value.into()],
                "prefix" => agent.role.config.command_prefix = vec![value.into()],
                "id" => agent.agent_id.name = value.into(),
                _ => (),
            }
            let result = launch_command(&agent, |key| match (field, key) {
                ("bin_override", "ATM_SPAWN_BIN") | ("args_override", "ATM_SPAWN_ARGS") => {
                    Some(value.into())
                }
                _ => None,
            });
            if value == "a b" {
                assert!(result.is_ok(), "{field}: {result:?}");
            } else {
                refused(result, "printable ASCII");
            }
        }
    }
}

#[test]
fn up_safe_command_word_stays_bare() {
    for (harness, suffix) in [
        ("claude", "claude"),
        ("pi", "pi"),
        ("codex", "codex '--no-daemon'"),
    ] {
        let command = launch_command(&prepared(harness), |_| None).unwrap();
        assert_eq!(
            command.text,
            format!("cd '/work' && ATM_AGENT='project/lead' {suffix}")
        );
        assert_eq!(command.binary, harness);
    }
    for (binary, word) in [
        ("my-agent_1.2", "my-agent_1.2"),
        ("./bin/agent", "./bin/agent"),
        ("/bin/test agent", "'/bin/test agent'"),
        ("it's", "'it'\\''s'"),
        ("-agent", "'-agent'"),
        ("agent;other", "'agent;other'"),
        ("$BIN", "'$BIN'"),
    ] {
        let command = launch_command(&prepared("pi"), |key| {
            (key == "ATM_SPAWN_BIN").then(|| binary.into())
        })
        .unwrap();
        assert_eq!(
            command.text,
            format!("cd '/work' && ATM_AGENT='project/lead' {word}")
        );
        assert_eq!(command.binary, binary);
    }
}

#[test]
fn up_models_use_launch_policy() {
    for (harness, suffix) in [
        ("claude", "claude --model 'model name'"),
        ("pi", "pi --model 'model name'"),
        ("codex", "codex '--no-daemon' -m 'model name'"),
    ] {
        let mut agent = prepared(harness);
        agent.role.config.model = Some("model name".into());
        assert_eq!(
            launch_command(&agent, |_| None).unwrap().text,
            format!("cd '/work' && ATM_AGENT='project/lead' {suffix}")
        );
    }
}

#[test]
fn up_unsupported_harness_refuses() {
    for harness in [
        Harness::Amp,
        Harness::Qwen,
        Harness::Gemini,
        Harness::Unknown,
    ] {
        let mut agent = prepared("pi");
        agent.role.config.harness = harness;
        refused(launch_command(&agent, |_| None), "launch policy");
    }
}

#[cfg(unix)]
#[test]
fn up_non_utf8_cwd_refuses() {
    use std::os::unix::ffi::OsStringExt;
    let mut agent = prepared("pi");
    agent.cwd = std::ffi::OsString::from_vec(b"/work/\xff".to_vec()).into();
    refused(launch_command(&agent, |_| None), "UTF-8");
}
