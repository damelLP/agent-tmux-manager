//! Synthetic owner-intent examples: no project measures are approved by these tests.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use atm_org::{parse_intent_measures, IntentError, TaskMeasure};
use proptest::prelude::*;

// Break caught: collecting measures from the whole document instead of the named section.
#[test]
fn only_the_measures_section_authorizes_measures() {
    let text = r#"# Project intent
Keep the cockpit working.
- not a measure
- mechanical: never-before

## How we will know
These are the owner's acceptance measures, not intermediate search metrics.
- mechanical: `cargo test --workspace`
- reviewed: `.atm/measures/structure.md`
- owner

## Anti-goals
- mechanical: never-after
"#;
    assert_eq!(
        parse_intent_measures(text).unwrap(),
        vec![
            TaskMeasure::Mechanical {
                command: "cargo test --workspace".into(),
            },
            TaskMeasure::Reviewed {
                rubric: ".atm/measures/structure.md".into(),
            },
            TaskMeasure::Owner,
        ]
    );
}

// Break caught: fixed heading depth, exact-case matching, or ignoring closing ATX hashes.
#[test]
fn atx_heading_levels_case_and_closing_hashes_are_supported() {
    for heading in [
        "# How we will know",
        "## How we will know ##",
        "### HOW WE WILL KNOW",
        "#### How we will know ###   ",
        "##### how we will know",
        "###### How we will know",
        "   ##  How we will know  ##",
    ] {
        assert_eq!(
            parse_intent_measures(&format!("{heading}\n- owner\n")).unwrap(),
            vec![TaskMeasure::Owner],
            "{heading}"
        );
    }
}

// Break caught: treating a Setext heading as prose, or missing its section-ending level.
#[test]
fn setext_headings_start_and_end_sections() {
    for text in [
        "How we will know\n================\n- owner\nNext\n====\n- unknown\n",
        "How we will know\n----------------\n- owner\nNext\n----\n- unknown\n",
        "  How we will know\n  ---\n- owner\n# Next\n- unknown\n",
    ] {
        assert_eq!(
            parse_intent_measures(text).unwrap(),
            vec![TaskMeasure::Owner]
        );
    }
}

// Break caught: substring heading matching or accepting prose/code as a heading.
#[test]
fn near_match_and_non_markdown_headings_do_not_start_a_section() {
    for heading in [
        "## How we will know later",
        "## Not How we will know",
        "#How we will know",
        "####### How we will know",
        "## How we will know###",
        "> ## How we will know",
        "    ## How we will know",
        "\t## How we will know",
        "How we will know",
    ] {
        assert_eq!(
            parse_intent_measures(&format!("{heading}\n- owner\n")),
            Err(IntentError::MissingSection),
            "{heading:?}"
        );
    }
}

// Break caught: stopping at every nested heading, or leaking into the next peer section.
#[test]
fn nested_subsections_belong_to_the_measures_section() {
    let text = "## How we will know\n### Correctness\n- mechanical: scripts/check\n#### Structure\n- reviewed: .atm/measures/structure.md\n## After\n- unknown\n### More\n- owner\n";
    assert_eq!(
        parse_intent_measures(text).unwrap(),
        vec![
            TaskMeasure::Mechanical {
                command: "scripts/check".into(),
            },
            TaskMeasure::Reviewed {
                rubric: ".atm/measures/structure.md".into(),
            },
        ]
    );
}

// Break caught: recognizing only '-' bullets or only numbered lists with a dot.
#[test]
fn markdown_list_markers_and_indentation_are_supported() {
    for marker in ["-", "*", "+", "1.", "2)", "123456789."] {
        for indent in ["", " ", "  ", "   "] {
            assert_eq!(
                parse_intent_measures(&format!("## How we will know\n{indent}{marker}\towner\n"))
                    .unwrap(),
                vec![TaskMeasure::Owner],
                "{indent:?}{marker}"
            );
        }
    }
}

// Break caught: normalizing command contents, losing Unicode/CRLF, or mishandling code delimiters.
#[test]
fn values_preserve_internal_bytes_and_remove_only_outer_code_delimiters() {
    let text = "## How we will know\r\n- mechanical:   printf  'déjà vu'  \r\n- mechanical: ``printf '%s' '`'``\r\n- reviewed: `.atm/measures/qualité.md`\r\n";
    assert_eq!(
        parse_intent_measures(text).unwrap(),
        vec![
            TaskMeasure::Mechanical {
                command: "printf  'déjà vu'".into(),
            },
            TaskMeasure::Mechanical {
                command: "printf '%s' '`'".into(),
            },
            TaskMeasure::Reviewed {
                rubric: ".atm/measures/qualité.md".into(),
            },
        ]
    );
}

// Break caught: allowing headings or measures from fenced examples.
#[test]
fn fenced_examples_cannot_authorize_a_heading_or_a_measure() {
    for fence in ["```", "````", "~~~", "~~~~"] {
        let text = format!(
            "{fence}markdown\n## How we will know\n- mechanical: never\n{fence}\n## How we will know\n- owner\n{fence}\n- unknown\n## How we will know\n{fence}\n"
        );
        assert_eq!(
            parse_intent_measures(&text).unwrap(),
            vec![TaskMeasure::Owner]
        );
        assert_eq!(
            parse_intent_measures(&format!(
                "{fence}\nHow we will know\n---\n- owner\n{fence}\n"
            )),
            Err(IntentError::MissingSection)
        );
    }
}

// Break caught: ending a fence on the wrong character, a shorter run, or trailing text.
#[test]
fn fences_end_only_on_a_matching_run_without_trailing_text() {
    let text = "## How we will know\n- owner\n````markdown\n```\n- unknown\n~~~~\n- unknown\n````not-a-close\n- unknown\n`````   \n- reviewed: .atm/measures/real.md\n";
    assert_eq!(
        parse_intent_measures(text).unwrap(),
        vec![
            TaskMeasure::Owner,
            TaskMeasure::Reviewed {
                rubric: ".atm/measures/real.md".into(),
            },
        ]
    );
}

// Break caught: trim_start turning an indented code example into approved measures.
#[test]
fn indented_code_cannot_authorize_measures() {
    let text = "## How we will know\n- owner\n    - mechanical: never\n\t- reviewed: .atm/measures/never.md\n    ## How we will know\n";
    assert_eq!(
        parse_intent_measures(text).unwrap(),
        vec![TaskMeasure::Owner]
    );
}

// Break caught: silently accepting an absent heading as an empty unrestricted policy.
#[test]
fn missing_section_is_an_explicit_error() {
    for text in [
        "",
        "# Goals\n- owner\n",
        "We discuss How we will know here.\n",
    ] {
        assert_eq!(
            parse_intent_measures(text),
            Err(IntentError::MissingSection)
        );
    }
}

// Break caught: silently accepting an empty section or examples as approved measures.
#[test]
fn empty_section_is_an_explicit_error() {
    for text in [
        "## How we will know\n",
        "## How we will know\nNothing approved yet.\n## Next\n- owner\n",
        "## How we will know\n```\n- owner\n```\n",
    ] {
        assert_eq!(parse_intent_measures(text), Err(IntentError::EmptySection));
    }
}

// Break caught: merging multiple independently located approval sections.
#[test]
fn duplicate_sections_are_refused_even_after_a_section_ends() {
    for text in [
        "## How we will know\n- owner\n## How we will know\n- owner\n",
        "## How we will know\n- owner\n### HOW WE WILL KNOW\n- owner\n",
        "# How we will know\n- owner\n# Elsewhere\nHow we will know\n---\n- owner\n",
    ] {
        assert!(matches!(
            parse_intent_measures(text),
            Err(IntentError::DuplicateSection { .. })
        ));
    }
}

// Break caught: ignoring malformed entries and returning a partially accepted list.
#[test]
fn malformed_entries_fail_the_whole_section_with_their_line() {
    for entry in [
        "owner: yes",
        "unknown",
        "mechanical:",
        "mechanical:   ",
        "mechanical: ``",
        "mechanical: `   `",
        "mechanical: `cargo test",
        "mechanical: ``cargo test`",
        "mechanical cargo test",
        "Mechanical: cargo test",
        "reviewed:",
        "mechanical: check\0bad",
    ] {
        assert_eq!(
            parse_intent_measures(&format!("## How we will know\n- owner\n- {entry}\n")),
            Err(IntentError::InvalidMeasure { line: 3 }),
            "{entry:?}"
        );
    }
}

// Break caught: reviewed rubric references escaping the protected measures directory.
#[test]
fn reviewed_paths_must_be_canonical_files_below_the_measures_directory() {
    for path in [
        "/tmp/rubric.md",
        "docs/rubric.md",
        ".atm/measures",
        ".atm/measures/",
        ".atm/measures/../intent.md",
        ".atm/measures/nested/../../rules.md",
        ".atm/measures/./rubric.md",
        ".atm/measures//rubric.md",
        ".atm/measures/rubric.md/",
        ".atm/measures/dir\\rubric.md",
    ] {
        assert_eq!(
            parse_intent_measures(&format!("## How we will know\n- reviewed: {path}\n")),
            Err(IntentError::InvalidMeasure { line: 2 }),
            "{path:?}"
        );
    }
    assert_eq!(
        parse_intent_measures("## How we will know\n- reviewed: .atm/measures/nested/rubric.md\n")
            .unwrap(),
        vec![TaskMeasure::Reviewed {
            rubric: ".atm/measures/nested/rubric.md".into(),
        }]
    );
}

// Break caught: duplicate entries, sorting the owner's list, or collapsing distinct command bytes.
#[test]
fn duplicates_are_removed_without_normalizing_different_measures() {
    assert_eq!(
        parse_intent_measures(
            "## How we will know\n- mechanical: check\n- mechanical: `check`\n- mechanical: check  now\n- mechanical: check now\n- owner\n- owner\n- reviewed: .atm/measures/structure.md\n- reviewed: `.atm/measures/structure.md`\n"
        ).unwrap(),
        vec![
            TaskMeasure::Mechanical { command: "check".into() },
            TaskMeasure::Mechanical { command: "check  now".into() },
            TaskMeasure::Mechanical { command: "check now".into() },
            TaskMeasure::Owner,
            TaskMeasure::Reviewed { rubric: ".atm/measures/structure.md".into() },
        ]
    );
}

// Break caught: mistaking Markdown thematic breaks for malformed measure bullets.
#[test]
fn prose_and_thematic_breaks_do_not_change_the_measure_list() {
    let text =
        "## How we will know\nOwner approval remains necessary.\n\n- owner\n\n---\n* * *\n___\n";
    assert_eq!(
        parse_intent_measures(text).unwrap(),
        vec![TaskMeasure::Owner]
    );
}

proptest! {
    #![proptest_config(ProptestConfig { failure_persistence: None, ..ProptestConfig::default() })]

    // Break caught: any byte-offset/splitting panic on arbitrary owner text.
    #[test]
    fn arbitrary_unicode_text_never_panics(text in "(?s).{0,2048}") {
        let _ = parse_intent_measures(&text);
    }

    // Break caught: shortening, normalizing, or changing the kind of an approved command.
    #[test]
    fn canonical_commands_keep_their_exact_value(command in "[a-zA-Z0-9_ -]{1,100}") {
        let command = command.trim();
        prop_assume!(!command.is_empty());
        prop_assert_eq!(
            parse_intent_measures(&format!("## How we will know\n- mechanical: {command}\n")).unwrap(),
            vec![TaskMeasure::Mechanical { command: command.into() }]
        );
    }
}
