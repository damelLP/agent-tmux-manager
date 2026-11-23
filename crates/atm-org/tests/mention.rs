use atm_org::mention::{parse_mentions, MentionToken};
use std::ops::Range;

type Expected<'a> = (Option<&'a str>, &'a str, Range<usize>);

fn assert_tokens(body: &str, expected: &[Expected<'_>]) {
    let actual = parse_mentions(body);
    assert_eq!(actual.len(), expected.len(), "{body:?}");
    for (token, (project, name, span)) in actual.iter().zip(expected) {
        assert_eq!(
            (token.project, token.name, &token.span),
            (*project, *name, span),
            "{body:?}"
        );
    }
}

fn assert_short_spellings(body: &str, expected: &[&str]) {
    let actual: Vec<_> = parse_mentions(body)
        .into_iter()
        .map(|token| {
            assert_eq!(token.project, None, "{body:?}");
            let Some(spelling) = body.get(token.span.clone()) else {
                panic!("invalid span {:?} in {body:?}", token.span);
            };
            assert_eq!(spelling.strip_prefix('@'), Some(token.name), "{body:?}");
            spelling
        })
        .collect();
    assert_eq!(actual, expected, "{body:?}");
}

#[test]
fn short_and_long_mentions_keep_order_and_byte_spans() {
    assert_tokens(
        "@lead, @a/b @lead",
        &[
            (None, "lead", 0..5),
            (Some("a"), "b", 7..11),
            (None, "lead", 12..17),
        ],
    );
    assert_tokens("", &[]);
    assert_tokens("@", &[]);
    assert_tokens(
        "@owner @system @p/owner",
        &[
            (None, "owner", 0..6),
            (None, "system", 7..14),
            (Some("p"), "owner", 15..23),
        ],
    );
}

#[test]
fn opening_punctuation_and_trailing_punctuation() {
    for body in ["(@a)", "[@a]", "{@a}", "<@a>", "'@a'", "\"@a\""] {
        assert_tokens(body, &[(None, "a", 1..3)]);
    }
    for body in ["“@a”", "‘@a’", "‹@a›"] {
        assert_tokens(body, &[(None, "a", 3..5)]);
    }
    assert_tokens("«@a»", &[(None, "a", 2..4)]);
    assert_tokens("@a.!", &[(None, "a", 0..2)]);
    for body in ["x)@a", "x:@a", "x’@a", "/@a", r"\@a"] {
        assert_tokens(body, &[]);
    }
}

#[test]
fn rejects_embedded_and_malformed_spellings_without_prefixes() {
    for body in [
        "user@host",
        "git@github.com:x/y",
        "@@a",
        "@-a",
        "@A",
        "@aB",
        "@a_name",
        "@a@host",
        "@a/",
        "@/b",
        "@a//b",
        "@a/-b",
        "@a/b/c",
    ] {
        assert_tokens(body, &[]);
    }
    assert_tokens("@0 @a--b-", &[(None, "0", 0..2), (None, "a--b-", 3..9)]);
    assert_tokens("@a/b/c, @good", &[(None, "good", 8..13)]);
}

#[test]
fn validates_each_slug_at_the_32_byte_limit() {
    let p = "a".repeat(32);
    let n = "b".repeat(32);
    assert_tokens(&format!("@{p}"), &[(None, &p, 0..33)]);
    assert_tokens(&format!("@{p}/{n}"), &[(Some(&p), &n, 0..66)]);
    for body in [format!("@{p}a"), format!("@{p}a/{n}"), format!("@{p}/{n}b")] {
        assert_tokens(&body, &[]);
    }
}

#[test]
fn unicode_whitespace_and_non_ascii_neighbours() {
    assert_tokens(
        "\u{2003}@a\u{00a0}@b",
        &[(None, "a", 3..5), (None, "b", 7..9)],
    );
    for body in ["字@a", "@aé", "@cafe\u{0301}", "@a🙂"] {
        assert_tokens(body, &[]);
    }
}

#[test]
fn skips_matched_backtick_spans() {
    assert_tokens(
        "@a `@types/node` @b",
        &[(None, "a", 0..2), (None, "b", 17..19)],
    );
    for body in [
        "@a `@types/node` @b",
        "@a `` @skip `\n@still `` @b",
        "` @a `` @skip `` @b",
        "@a ` @skip `` @still ` @b",
    ] {
        assert_short_spellings(body, &["@a", "@b"]);
    }
    assert_short_spellings("` @a ` @b ` @c ` @d", &["@b", "@d"]);
}

#[test]
fn skips_fenced_blocks_and_resumes_only_after_a_valid_close() {
    assert_short_spellings(
        "@a\n````rust @info\n@inside\n```\n@still\n````\n@b",
        &["@a", "@b"],
    );
    assert_short_spellings(
        "  ~~~~lang @info\r\n@inside\r\n~~~~ not-a-close\r\n@still\r\n````\r\n@deeper\r\n~~~~~\r\n@b",
        &["@b"],
    );
    assert_short_spellings("@a\n~~~\n@hidden", &["@a"]);
    assert_short_spellings("` @a\n   ~~~\n @hidden\n   ~~~ \t\n` @b", &["@a", "@b"]);
    assert_short_spellings("    ~~~\n@a", &["@a"]);
}

#[test]
fn line_start_backticks_with_a_backtick_tail_are_inline() {
    assert_tokens(
        "```code``` @a\n@b",
        &[(None, "a", 11..13), (None, "b", 14..16)],
    );
    assert_short_spellings("   ``` @hidden ``` @a\n@b", &["@a", "@b"]);
    assert_short_spellings("```info ` @a\n@b", &["@a", "@b"]);
    assert_short_spellings("```info `\n@hidden ``` @a", &["@a"]);
}

#[test]
fn a_different_marker_does_not_close_a_fence() {
    for body in [
        "~~~lang ` @info\n@hidden\n```\n@still\n~~~\n@a",
        "```rust\n@hidden\n~~~\n@still\n```\n@a",
    ] {
        assert_short_spellings(body, &["@a"]);
    }
}

#[test]
fn a_shorter_run_does_not_close_a_fence() {
    assert_short_spellings("````\n@hidden\n```\n@still\n`````\n@a", &["@a"]);
    assert_short_spellings("prose ``` @a\n```\n@hidden", &["@a"]);
    assert_short_spellings("prose ``` @a\n``` @info", &["@a"]);
}

#[test]
fn an_extra_indented_run_does_not_close_a_fence() {
    assert_short_spellings("```\n@hidden\n    ```\n@still\n   ```\n@a", &["@a"]);
}

#[test]
fn long_backtick_runs_keep_surrounding_byte_spans() {
    let ticks = "`".repeat(4096);
    assert_tokens(
        &format!("@a {ticks} @hidden {ticks} @b"),
        &[(None, "a", 0..2), (None, "b", 8205..8207)],
    );
}

#[test]
fn dense_16_kib_body_has_exact_tokens() {
    let body = "@@@@ @a @p/name\n".repeat(1024);
    assert_eq!(body.len(), 16_384);
    let tokens = parse_mentions(&body);
    assert_eq!(tokens.len(), 2048);
    for (i, pair) in tokens.as_chunks::<2>().0.iter().enumerate() {
        let start = 16 * i;
        assert_eq!(
            pair,
            &[
                MentionToken {
                    project: None,
                    name: "a",
                    span: start + 5..start + 7,
                },
                MentionToken {
                    project: Some("p"),
                    name: "name",
                    span: start + 8..start + 15,
                },
            ],
            "block {i}"
        );
    }
}
