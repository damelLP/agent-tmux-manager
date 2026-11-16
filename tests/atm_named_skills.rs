//! Distribution contracts for the named skills, not model-compliance tests.
//! K3's billed behavioral evaluations require separate approval. These checks
//! catch unusable metadata, broken installed references and missing evidence slots.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

const SKILLS: [&str; 2] = ["atm-lead", "atm-member"];
const REFERENCE: &str = "references/atm-named-agents.md";

fn source() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("crates/atm/assets/atm-skills")
}

fn document(skill: &str) -> String {
    fs::read_to_string(source().join(skill).join("SKILL.md"))
        .unwrap_or_else(|error| panic!("missing distributable {skill}/SKILL.md: {error}"))
}

fn field<'a>(text: &'a str, key: &str) -> &'a str {
    text.lines()
        .find_map(|line| line.strip_prefix(key))
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| panic!("missing nonempty {key} field"))
}

fn links(text: &str) -> impl Iterator<Item = &str> {
    text.split("](")
        .skip(1)
        .map(|rest| rest.split_once(')').expect("closed Markdown link").0)
}

// Break caught: a folder/name mismatch or implicit-only metadata makes explicit
// bootstrap select the wrong resource or exposes unattended selection.
#[test]
fn named_skill_metadata_matches_its_folder_and_is_explicit_only() {
    for skill in SKILLS {
        let text = document(skill);
        let (front, _) = text
            .strip_prefix("---\n")
            .and_then(|rest| rest.split_once("\n---\n"))
            .expect("portable YAML frontmatter");
        assert_eq!(field(front, "name:"), skill);
        let description = field(front, "description:");
        assert!(description.starts_with("Use when"));
        assert!(description.chars().count() <= 1024);
        assert_eq!(field(front, "disable-model-invocation:"), "true");
    }
    let policy = fs::read_to_string(source().join("shared/agents/openai.yaml")).unwrap();
    assert_eq!(field(&policy, "  allow_implicit_invocation:"), "false");
}

// Break caught: adding a rule without an evidence record leaves its reason or
// verification status unavailable to an installed skill's reader. This checks
// the record schema, not the wording or truth of a model-compliance claim.
#[test]
fn every_named_rule_has_reason_source_and_verification() {
    let reference = fs::read_to_string(source().join("shared").join(REFERENCE))
        .expect("distributable named-agent reference");
    let (_, catalogue) = reference
        .split_once("## Verification records\n")
        .expect("rule catalogue");
    let mut records = BTreeSet::new();
    for section in catalogue.split("\n### ").skip(1) {
        let (id, body) = section.split_once('\n').expect("rule record body");
        assert!(records.insert(id.to_lowercase()), "duplicate record {id}");
        field(body, "**Why:**");
        field(body, "**Source:**");
        let checked = field(body, "**Checked:**");
        for key in ["harness:", "model:"] {
            assert!(checked.split(';').any(|entry| entry
                .trim()
                .strip_prefix(key)
                .is_some_and(|value| !value.trim().is_empty())));
        }
        field(body, "**Not verified:**");
    }
    let wanted: BTreeSet<_> = (1..=7)
        .map(|n| format!("s{n}"))
        .chain((1..=7).map(|n| format!("m{n}")))
        .chain((1..=9).map(|n| format!("l{n}")))
        .collect();
    assert_eq!(records, wanted);
    let used: BTreeSet<_> = SKILLS
        .iter()
        .flat_map(|skill| {
            let text = document(skill);
            links(&text)
                .filter_map(|link| link.strip_prefix(&format!("{REFERENCE}#")))
                .map(str::to_owned)
                .collect::<Vec<_>>()
        })
        .collect();
    assert_eq!(used, records, "every rule is reachable from a skill");
}

// Break caught: a shared asset exists in source but a skill links to the wrong
// installed-relative path or a nonexistent anchor. Setup tests separately prove
// the embedded manifest actually distributes all these source files unchanged.
#[test]
fn named_skill_references_resolve_in_the_installed_layout() {
    for skill in SKILLS {
        for link in links(&document(skill)) {
            let (path, anchor) = link.split_once('#').unwrap_or((link, ""));
            let direct = source().join(skill).join(path);
            let target = if direct.is_file() {
                direct
            } else {
                source().join("shared").join(path)
            };
            assert!(target.is_file(), "{skill}: broken resource {link}");
            assert!(target.canonicalize().unwrap().starts_with(source()));
            if !anchor.is_empty() {
                let text = fs::read_to_string(&target).unwrap();
                assert!(
                    text.lines().any(|line| line
                        .strip_prefix("### ")
                        .is_some_and(|heading| heading.to_lowercase() == anchor)),
                    "{skill}: broken anchor {link}"
                );
            }
        }
    }
}

// Break caught: a distributed entry example is too large for the recorded
// Claude slash-command boundary or names a different skill. This checks the
// example size, not the unverified current-harness paste boundary itself.
#[test]
fn claude_bootstrap_examples_fit_the_recorded_skill_call_limit() {
    for skill in SKILLS {
        let text = document(skill);
        let (_, rest) = text.split_once("```text\n").expect("entry examples");
        let (examples, _) = rest.split_once("\n```").expect("closed examples");
        let lines: Vec<_> = examples.lines().collect();
        assert_eq!(
            lines,
            [
                format!("/{skill}"),
                format!("/skill:{skill}"),
                format!("${skill}")
            ]
        );
        assert!(lines.iter().all(|line| line.chars().count() <= 800));
        assert!(lines.len() <= 3);
    }
}

// Break caught: an evidence catalogue is shipped but a skill omits one of its
// instruction blocks (for example a lead still has only a scaffold of links).
// This validates the rule-ID structure, not compliance with the prose.
#[test]
fn named_skills_expose_each_rule_as_an_instruction() {
    for (skill, prefix, count) in [("atm-member", 'M', 7), ("atm-lead", 'L', 9)] {
        let text = document(skill);
        let actual: Vec<_> = text
            .lines()
            .filter_map(|line| line.strip_prefix("- **["))
            .map(|rest| rest.split_once("](").expect("linked rule").0.to_owned())
            .collect();
        let wanted: BTreeSet<_> = (1..=7)
            .map(|n| format!("S{n}"))
            .chain((1..=count).map(|n| format!("{prefix}{n}")))
            .collect();
        assert_eq!(actual.len(), wanted.len(), "{skill}: rule blocks");
        assert_eq!(
            actual.into_iter().collect::<BTreeSet<_>>(),
            wanted,
            "{skill}"
        );
    }
}
