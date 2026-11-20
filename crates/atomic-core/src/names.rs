// SPDX-License-Identifier: Apache-2.0

//! The nearest name, for a "did you mean" when a reference names nothing.

/// The edits, inserting, deleting or replacing one character or swapping
/// two next to each other, that turn `a` into `b`.
fn distance(a: &str, b: &str) -> usize {
    let (a, b): (Vec<char>, Vec<char>) = (a.chars().collect(), b.chars().collect());
    let mut d = vec![vec![0usize; b.len() + 1]; a.len() + 1];
    for (i, row) in d.iter_mut().enumerate() {
        row[0] = i;
    }
    for (j, cell) in d[0].iter_mut().enumerate() {
        *cell = j;
    }
    for i in 1..=a.len() {
        for j in 1..=b.len() {
            let cost = usize::from(a[i - 1] != b[j - 1]);
            let mut best = (d[i - 1][j] + 1)
                .min(d[i][j - 1] + 1)
                .min(d[i - 1][j - 1] + cost);
            if i > 1 && j > 1 && a[i - 1] == b[j - 2] && a[i - 2] == b[j - 1] {
                best = best.min(d[i - 2][j - 2] + 1);
            }
            d[i][j] = best;
        }
    }
    d[a.len()][b.len()]
}

/// The candidate `name` most likely meant: the fewest edits away, and close
/// enough to be a slip: one edit for a name of three or four characters,
/// otherwise at most two edits or a third of the name. A name of one or two
/// characters is one edit from too many others to guess. A tie goes to the
/// first in alphabetical order. `None` when nothing is that close.
pub fn nearest<'a>(name: &str, candidates: impl IntoIterator<Item = &'a str>) -> Option<&'a str> {
    let length = name.chars().count();
    if length <= 2 {
        return None;
    }
    // Two edits turn most short names into most others.
    let limit = if length <= 4 { 1 } else { 2.max(length / 3) };
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

#[cfg(test)]
mod tests {
    use super::nearest;

    #[test]
    fn a_slip_is_found_and_a_different_short_name_is_not() {
        let names = ["count", "rank", "summarize", "ci", "lint"];
        assert_eq!(nearest("summarise", names), Some("summarize"));
        assert_eq!(nearest("cuont", names), Some("count"));
        assert_eq!(nearest("rnk", names), Some("rank"));
        // Two edits from a short name is another name, not a slip.
        assert_eq!(nearest("xy", names), None);
        assert_eq!(nearest("link", names), Some("lint"));
        assert_eq!(nearest("tank", ["rank"]), Some("rank"));
        // Two characters swapped is one slip.
        assert_eq!(nearest("atmo", ["atom", "after"]), Some("atom"));
        assert_eq!(nearest("task", ["rank"]), None);
        assert_eq!(nearest("", names), None);
        // Nor is anything one edit from a two-character name.
        assert_eq!(nearest("cx", names), None);
    }
}
