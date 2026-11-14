use atm_core::Harness;
use atm_org::*;

#[test]
fn folder_slugs_and_project_roundtrip() {
    for (folder, expected) in [
        ("My Repo", "my-repo"),
        ("---A___B---", "a-b"),
        ("", "project"),
        ("日本語", "project"),
    ] {
        let file = new_project_file(folder);
        assert_eq!(file.id.to_string(), expected);
        assert_eq!(parse_project(&file.to_toml().unwrap()).unwrap(), file);
    }
    assert_eq!(
        new_project_file(&"A".repeat(40)).id.to_string(),
        "a".repeat(32)
    );
    for valid in ["-", "a--b-", "0"] {
        assert!(valid.parse::<ProjectId>().is_ok());
    }
    for invalid in ["", "My-Repo", "a/b", "a b", &"a".repeat(33)] {
        assert!(invalid.parse::<ProjectId>().is_err());
    }
    assert!(parse_project("id='a'\ntypo=1")
        .unwrap_err()
        .to_string()
        .contains("typo"));
    assert!(parse_project("").is_err());
}

#[test]
fn identities_validate_text_and_serde() {
    let agent: AgentId = "my-repo/lead".parse().unwrap();
    assert_eq!(agent.to_string(), "my-repo/lead");
    assert_eq!(serde_json::to_string(&agent).unwrap(), "\"my-repo/lead\"");
    for name in [
        "",
        "a/b",
        "a b",
        "a\tb",
        "a\u{2003}b",
        "a\0b",
        "Lead",
        "é",
        "-a",
        "owner",
        "system",
        ".",
        "..",
        &"a".repeat(33),
    ] {
        assert!(
            AgentId::new("p".parse().unwrap(), name).is_err(),
            "{name:?}"
        );
        assert!(serde_json::from_value::<AgentId>(serde_json::json!(format!("p/{name}"))).is_err());
    }
    for text in ["p/lead/other", "lead", "/lead"] {
        assert!(text.parse::<AgentId>().is_err());
    }
    for text in ["owner", "system", "p/lead"] {
        assert_eq!(text.parse::<Address>().unwrap().to_string(), text);
    }
    assert!("lead".parse::<Address>().is_err());
    for seq in [0, 1, u64::MAX] {
        let id = MessageId::new(seq);
        assert_eq!(id.to_string().parse::<MessageId>().unwrap().seq(), seq);
        assert_eq!(
            serde_json::from_value::<MessageId>(serde_json::json!(id.to_string())).unwrap(),
            id
        );
    }
    for text in ["1", "m", "m01", "m-1", "m+1", "m18446744073709551616"] {
        assert!(text.parse::<MessageId>().is_err());
    }
    assert!("".parse::<TaskId>().is_err());
}

const ROLE: &str = "+++\nharness = \"pi\"\nmodel = \"gpt-6.1-sol\"\ncwd = \".worktrees/alpha\"\nmanager = \"lead\"\nprofile = \"pr-implementer\"\nskills = [\"atm-implement\"]\nspawn_args = []\ncommand_prefix = []\npane = \"below:lead\"\nclear = \"task\"\ntrust_dirs = [\".worktrees/alpha\"]\n+++\nStanding instructions for this role.\n";

#[test]
fn parses_the_brief_role_example() {
    let role = parse_role("alpha", ROLE).unwrap();
    assert_eq!(role.name, "alpha");
    assert_eq!(role.config.harness, Harness::Pi);
    assert_eq!(role.config.model.as_deref(), Some("gpt-6.1-sol"));
    assert_eq!(role.config.cwd, ".worktrees/alpha");
    assert_eq!(role.config.manager.as_deref(), Some("lead"));
    assert_eq!(role.config.profile.as_deref(), Some("pr-implementer"));
    assert_eq!(role.config.skills, ["atm-implement"]);
    assert!(role.config.spawn_args.is_empty());
    assert!(role.config.command_prefix.is_empty());
    assert_eq!(role.config.pane.as_deref(), Some("below:lead"));
    assert_eq!(role.config.clear.as_deref(), Some("task"));
    assert_eq!(role.config.trust_dirs, [".worktrees/alpha"]);
    assert_eq!(role.instructions, "Standing instructions for this role.\n");
    let value = serde_json::to_value(role).unwrap();
    assert_eq!(value["harness"], "pi");
    assert!(value.get("config").is_none());
}

#[test]
fn role_errors_name_the_problem() {
    for (text, problem) in [
        ("+++\n+++", "harness"),
        ("+++\nharness='pi'\ntyop=1\n+++", "tyop"),
        ("+++\nharness='pi'", "unterminated"),
        ("harness='pi'", "opening"),
        ("+++\nharness='typo'\n+++", "typo"),
        ("+++\nharness=[\n+++", "TOML"),
    ] {
        let error = parse_role("alpha", text).unwrap_err().to_string();
        assert!(error.contains(problem), "{error}");
    }
    assert!(parse_role("owner", ROLE).is_err());
}

#[test]
fn role_preserves_body_and_defaults_and_registry_aliases() {
    let role = parse_role(
        "lead",
        "+++\r\nharness='claude_code'\r\n+++\r\n\r\n Text \r\n+++\r\n",
    )
    .unwrap();
    assert_eq!(role.instructions, "\r\n Text \r\n+++\r\n");
    assert_eq!(role.config.harness, Harness::ClaudeCode);
    assert_eq!(role.config.cwd, ".");
    assert!(role.config.skills.is_empty());
    assert!(role.config.model.is_none());
    assert_eq!(
        parse_role("lead", "+++\nharness='claude'\n+++")
            .unwrap()
            .config
            .harness,
        Harness::ClaudeCode
    );
    assert!(parse_role("lead", "+++\nharness='pi'\n+++\n")
        .unwrap()
        .instructions
        .is_empty());
}
