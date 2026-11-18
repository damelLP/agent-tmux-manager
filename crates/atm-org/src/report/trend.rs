//! Full newest-anchored buckets and lower medians, never a composite score.
use super::{Direction, Outcome, TaskReport, Trend, TrendBucket, TrendDirection};
use crate::TaskKind;
use std::cmp::Ordering;
fn median<T: Copy + Ord>(values: impl Iterator<Item = T>) -> Option<T> {
    let mut values: Vec<_> = values.collect();
    values.sort();
    values.get(values.len().saturating_sub(1) / 2).copied()
}
fn direction<T: Ord>(before: Option<T>, after: Option<T>) -> Direction {
    match before.zip(after).map(|(a, b)| b.cmp(&a)) {
        Some(Ordering::Greater) => Direction::Up,
        Some(Ordering::Less) => Direction::Down,
        Some(Ordering::Equal) => Direction::Flat,
        None => Direction::None,
    }
}
pub(super) fn trend<'a>(rows: impl Iterator<Item = &'a TaskReport>, bucket: u16) -> Trend {
    let mut rows: Vec<_> = rows
        .filter(|t| t.kind == TaskKind::Work && matches!(t.outcome, Outcome::Accepted))
        .filter_map(|t| t.ended_at.map(|at| (at, t)))
        .collect();
    // Stable sorting preserves numeric creation order for equal acceptance times.
    rows.sort_by_key(|(at, _)| *at);
    let size = usize::from(bucket.max(1));
    let full = rows.get(rows.len() % size..).unwrap_or_default();
    let buckets: Vec<_> = full
        .chunks_exact(size)
        .filter_map(|group| {
            let from = group.first()?.0;
            let to = group.last()?.0;
            Some(TrendBucket {
                from,
                to,
                n: bucket,
                turns: median(group.iter().filter_map(|(_, t)| t.usage.turns)),
                peak_context_percent: median(
                    group
                        .iter()
                        .filter_map(|(_, t)| t.usage.peak_context_percent),
                ),
                review_rounds: median(group.iter().filter_map(|(_, t)| t.usage.review_rounds)),
                changes_requested: median(
                    group.iter().filter_map(|(_, t)| t.usage.changes_requested),
                ),
                cost: median(group.iter().filter_map(|(_, t)| t.usage.cost)),
                cost_known: u16::try_from(
                    group.iter().filter(|(_, t)| t.usage.cost.is_some()).count(),
                )
                .unwrap_or(u16::MAX),
            })
        })
        .collect();
    let mut directions = TrendDirection::default();
    if let Some((a, b)) = buckets
        .len()
        .checked_sub(2)
        .and_then(|i| buckets.get(i).zip(buckets.last()))
    {
        directions = TrendDirection {
            turns: direction(a.turns, b.turns),
            peak_context_percent: direction(a.peak_context_percent, b.peak_context_percent),
            review_rounds: direction(a.review_rounds, b.review_rounds),
            changes_requested: direction(a.changes_requested, b.changes_requested),
            cost: direction(a.cost, b.cost),
        };
    }
    Trend {
        bucket,
        buckets,
        direction: directions,
    }
}
