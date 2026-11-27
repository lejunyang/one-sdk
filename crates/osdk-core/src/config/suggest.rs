use std::cmp::Ordering;

/// Optimal string alignment distance, including adjacent transpositions.
/// Configuration names are ASCII and compared without regard to case.
pub(super) fn distance(a: &str, b: &str) -> usize {
    let b: Vec<_> = b.chars().collect();
    let mut two_back = vec![0; b.len() + 1];
    let mut previous: Vec<_> = (0..=b.len()).collect();
    let mut current = vec![0; b.len() + 1];
    let mut previous_char: Option<char> = None;

    for (i, ca) in a.chars().enumerate() {
        current[0] = i + 1;
        for (j, cb) in b.iter().enumerate() {
            let substitution = previous[j] + usize::from(!ca.eq_ignore_ascii_case(cb));
            current[j + 1] = substitution.min(previous[j + 1] + 1).min(current[j] + 1);

            if j > 0
                && ca.eq_ignore_ascii_case(&b[j - 1])
                && previous_char.is_some_and(|c| c.eq_ignore_ascii_case(cb))
            {
                current[j + 1] = current[j + 1].min(two_back[j - 1] + 1);
            }
        }
        std::mem::swap(&mut two_back, &mut previous);
        std::mem::swap(&mut previous, &mut current);
        previous_char = Some(ca);
    }
    previous[b.len()]
}

/// Return a suggestion only when one candidate is strictly closer than the others.
pub(super) fn closest<'a>(
    written: &str,
    candidates: impl IntoIterator<Item = &'a str>,
    max: usize,
) -> Option<&'a str> {
    let mut best_distance = usize::MAX;
    let mut best = None;
    for candidate in candidates {
        let edits = distance(written, candidate);
        if edits > max {
            continue;
        }
        match edits.cmp(&best_distance) {
            Ordering::Less => {
                best_distance = edits;
                best = Some(candidate);
            }
            Ordering::Equal => best = None,
            Ordering::Greater => {}
        }
    }
    best
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn distance_counts_each_edit_and_ignores_ascii_case() {
        assert_eq!(distance("Apt", "apt"), 0);
        for written in ["atp", "ap", "aptt", "apu"] {
            assert_eq!(distance(written, "apt"), 1, "{written}");
        }
        assert_eq!(distance("atp", "apk"), 2);
    }

    #[test]
    fn a_suggestion_must_be_uniquely_closest_and_within_the_threshold() {
        assert_eq!(closest("ap", ["apt", "apk"], 1), None);
        assert_eq!(closest("ap", ["apk", "apt"], 1), None);
        assert_eq!(closest("atp", ["apk", "apt"], 1), Some("apt"));
        assert_eq!(closest("apt", ["apk", "ap", "apt"], 1), Some("apt"));
        assert_eq!(closest("yum", ["apt", "apk"], 1), None);
    }
}
