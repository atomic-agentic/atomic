// SPDX-License-Identifier: Apache-2.0

//! The nearest name, for a "did you mean" when a reference names nothing.

/// The edits, inserting, deleting or replacing one character, that turn
/// `a` into `b`.
fn distance(a: &str, b: &str) -> usize {
    let (a, b): (Vec<char>, Vec<char>) = (a.chars().collect(), b.chars().collect());
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    for i in 1..=a.len() {
        let mut cur = vec![i; b.len() + 1];
        for j in 1..=b.len() {
            let cost = usize::from(a[i - 1] != b[j - 1]);
            cur[j] = (prev[j] + 1).min(cur[j - 1] + 1).min(prev[j - 1] + cost);
        }
        prev = cur;
    }
    prev[b.len()]
}

/// The candidate `name` most likely meant: the fewest edits away, and close
/// enough to be a slip, at most two edits or a third of the name. A tie goes
/// to the first in order. `None` when nothing is that close.
pub fn nearest<'a>(name: &str, candidates: impl IntoIterator<Item = &'a str>) -> Option<&'a str> {
    if name.is_empty() {
        return None;
    }
    let limit = 2.max(name.chars().count() / 3);
    let mut best: Option<(usize, &'a str)> = None;
    for candidate in candidates {
        let d = distance(name, candidate);
        if d == 0 || d > limit {
            continue;
        }
        let better = match best {
            None => true,
            Some((bd, bc)) => d < bd || (d == bd && candidate < bc),
        };
        if better {
            best = Some((d, candidate));
        }
    }
    best.map(|(_, c)| c)
}
