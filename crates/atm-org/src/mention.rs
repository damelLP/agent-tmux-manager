//! Lexical mentions in channel post bodies, before any address resolution.

use crate::ids::slug_chars;
use std::ops::Range;

/// A mention borrowing the original post body, with no leading `@` in either part.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MentionToken<'a> {
    pub project: Option<&'a str>,
    pub name: &'a str,
    /// Half-open byte range of the whole token, including `@`.
    pub span: Range<usize>,
}

struct Fence {
    marker: char,
    width: usize,
}

enum Piece<'a> {
    Token(MentionToken<'a>),
    Ticks { next_same: Option<usize> },
}

/// Find lexical mentions in order, including duplicates, without resolving names.
///
/// Malformed spellings are rejected whole. Top-level backtick/tilde fences
/// (up to three leading spaces) and matched equal-width backtick spans are
/// skipped. A backtick opener with another backtick on its line is inline.
/// Unmatched inline runs are literal; an unclosed fence runs to EOF.
/// This is not a full Markdown parser (no nested blocks or escape processing).
#[must_use]
pub fn parse_mentions(body: &str) -> Vec<MentionToken<'_>> {
    let mut cursor = body.char_indices().peekable();
    let (mut previous, mut indent, mut fence) = (None, Some(0), None::<Fence>);
    let (mut out, mut pieces, mut last_tick) = (Vec::new(), Vec::new(), Vec::new());
    // Width and piece index of a possible backtick opener. Buffer its line as
    // prose until another backtick confirms it is inline, or LF confirms a fence.
    let mut tentative_fence = None;
    while let Some((start, ch)) = cursor.next() {
        let prev = previous.replace(ch);
        let line_prefix = indent.is_some();
        indent = match ch {
            '\n' => Some(0),
            ' ' => indent.filter(|&spaces| spaces < 3).map(|spaces| spaces + 1),
            _ => None,
        };

        if let ('\n', Some((width, index))) = (ch, tentative_fence) {
            pieces.truncate(index);
            emit_prose(&mut pieces, &mut out);
            last_tick.clear();
            fence = Some(Fence { marker: '`', width });
            tentative_fence = None;
            continue;
        }

        if (line_prefix && matches!(ch, '`' | '~')) || (ch == '`' && fence.is_none()) {
            let width = 1 + std::iter::from_fn(|| cursor.next_if(|&(_, c)| c == ch)).count();
            let fence_run = line_prefix && width >= 3;
            if fence_run && (ch == '~' || fence.is_some()) {
                let mut blank = true;
                while let Some((_, tail)) = cursor.next_if(|&(_, c)| c != '\n') {
                    blank &= tail.is_whitespace();
                }
                match &fence {
                    None => {
                        emit_prose(&mut pieces, &mut out);
                        last_tick.clear();
                        fence = Some(Fence { marker: ch, width });
                    }
                    Some(open) if open.marker == ch && width >= open.width && blank => {
                        fence = None;
                    }
                    Some(_) => {}
                }
            } else if ch == '`' && fence.is_none() {
                let current = pieces.len();
                if fence_run {
                    tentative_fence = Some((width, current));
                } else {
                    if let Some((width, index)) = tentative_fence.take() {
                        link_tick_run(&mut pieces, &mut last_tick, width, index);
                    }
                    link_tick_run(&mut pieces, &mut last_tick, width, current);
                }
                pieces.push(Piece::Ticks { next_same: None });
            }
        } else if ch == '@' && fence.is_none() && allows_mention_after(prev) {
            while let Some((_, part)) = cursor.next_if(|&(_, c)| !ends_candidate(c)) {
                previous = Some(part);
            }
            let end = cursor.peek().map_or(body.len(), |&(i, _)| i);
            if let Some(token) = validate_token(body, start, end) {
                pieces.push(Piece::Token(token));
            }
        }
    }
    if let Some((_, index)) = tentative_fence {
        pieces.truncate(index);
    }
    emit_prose(&mut pieces, &mut out);
    out
}

// Link equal runs during the scan: no search of the text from each opener.
fn link_tick_run(
    pieces: &mut [Piece<'_>],
    last_tick: &mut Vec<Option<usize>>,
    width: usize,
    index: usize,
) {
    if last_tick.len() <= width {
        last_tick.resize(width + 1, None);
    }
    let prior = last_tick
        .get_mut(width)
        .and_then(|slot| slot.replace(index));
    if let Some(Piece::Ticks { next_same }) = prior.and_then(|prior| pieces.get_mut(prior)) {
        *next_same = Some(index);
    }
}

fn allows_mention_after(prev: Option<char>) -> bool {
    prev.is_none_or(|c| {
        c.is_whitespace()
            || matches!(
                c,
                '(' | '[' | '{' | '<' | '\'' | '"' | '‘' | '“' | '«' | '‹'
            )
    })
}

fn ends_candidate(ch: char) -> bool {
    ch.is_whitespace()
        || (ch.is_ascii_punctuation() && !matches!(ch, '-' | '_' | '/' | '@'))
        || matches!(
            ch,
            '‘' | '’' | '“' | '”' | '«' | '»' | '‹' | '›' | '…' | '–' | '—'
        )
}

fn validate_token(body: &str, start: usize, end: usize) -> Option<MentionToken<'_>> {
    let spelling = body.get(start + 1..end)?;
    let (project, name) = match spelling.split_once('/') {
        Some((p, name)) if slug_chars(p) => (Some(p), name),
        Some(_) => return None,
        None => (None, spelling),
    };
    slug_chars(name).then_some(MentionToken {
        project,
        name,
        span: start..end,
    })
}

fn emit_prose<'a>(pieces: &mut Vec<Piece<'a>>, out: &mut Vec<MentionToken<'a>>) {
    // The first run with a closer wins; runs inside its code span are ignored.
    let mut through = None;
    for (index, piece) in pieces.drain(..).enumerate() {
        if through.is_some_and(|end| index <= end) {
            continue;
        }
        match piece {
            Piece::Token(token) => out.push(token),
            Piece::Ticks { next_same } => through = next_same,
        }
    }
}
