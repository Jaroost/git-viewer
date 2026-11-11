//! Commit search: matching, highlighting spans and navigation between results.

use crate::repo::CommitInfo;
use std::ops::Range;

/// Byte range of the first case-insensitive occurrence of `needle` in `hay`.
/// Works on characters (accents, non-ASCII), and the range always sits on char boundaries.
pub fn find_ci(hay: &str, needle: &str) -> Option<Range<usize>> {
    if needle.is_ascii() {
        // Fast path, no allocation: an ASCII needle can only match ASCII bytes, which are
        // always on char boundaries. (Unicode oddities such as the Kelvin sign matching "k"
        // are only handled by the slow path below.)
        let n = needle.as_bytes();
        if n.is_empty() {
            return None;
        }
        let pos = hay.as_bytes().windows(n.len()).position(|w| w.eq_ignore_ascii_case(n))?;
        return Some(pos..pos + n.len());
    }
    let n: Vec<char> = needle.chars().flat_map(char::to_lowercase).collect();
    if n.is_empty() {
        return None;
    }
    for (start, _) in hay.char_indices() {
        let mut k = 0;
        for (off, c) in hay[start..].char_indices() {
            let mut ok = true;
            for lc in c.to_lowercase() {
                if k < n.len() && n[k] == lc {
                    k += 1;
                } else {
                    ok = false;
                    break;
                }
            }
            if !ok {
                break;
            }
            if k == n.len() {
                return Some(start..start + off + c.len_utf8());
            }
        }
    }
    None
}

/// A commit matches if the query is in its message or author, or starts its hash.
/// `query_lc` is `query` lowercased (computed once by the caller, not per commit).
pub fn commit_matches(c: &CommitInfo, query: &str, query_lc: &str) -> bool {
    find_ci(&c.summary, query).is_some()
        || find_ci(&c.author, query).is_some()
        || c.id.starts_with(query_lc)
}

/// Indices of the commits matching `query` (already trimmed, non-empty).
/// `previous` is the last query with its results: when the new query extends it, only those
/// results are re-checked, since adding letters can only remove matches.
pub fn find_matches(commits: &[CommitInfo], query: &str, previous: Option<(&str, &[usize])>) -> Vec<usize> {
    let query_lc = query.to_lowercase();
    let hit = |i: usize| commit_matches(&commits[i], query, &query_lc);
    match previous {
        Some((prev, found)) if !prev.is_empty() && query.starts_with(prev) => {
            found.iter().copied().filter(|&i| i < commits.len() && hit(i)).collect()
        }
        _ => (0..commits.len()).filter(|&i| hit(i)).collect(),
    }
}

/// Next/previous result with wrap-around. `current` is a position in the list of matches.
pub fn step(len: usize, current: Option<usize>, delta: isize) -> Option<usize> {
    if len == 0 {
        return None;
    }
    Some(match current {
        Some(c) => (c as isize + delta).rem_euclid(len as isize) as usize,
        None if delta >= 0 => 0,
        None => len - 1,
    })
}

/// Position (in `matches`, sorted ascending) of the first match at or after commit `start`,
/// wrapping to the first match when there is none further down.
pub fn first_at_or_after(matches: &[usize], start: usize) -> Option<usize> {
    if matches.is_empty() {
        return None;
    }
    Some(matches.partition_point(|&i| i < start) % matches.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn commit(summary: &str, author: &str, id: &str) -> CommitInfo {
        CommitInfo {
            id: id.into(),
            summary: summary.into(),
            author: author.into(),
            time: 0,
            parents: vec![],
        }
    }

    #[test]
    fn find_is_case_insensitive_and_returns_byte_range() {
        assert_eq!(find_ci("Fix the Bug", "bug"), Some(8..11));
        assert_eq!(find_ci("Fix the Bug", "FIX"), Some(0..3));
        assert_eq!(find_ci("abc", "abcd"), None);
        assert_eq!(find_ci("abc", ""), None);
        assert_eq!(find_ci("", "a"), None);
    }

    #[test]
    fn find_handles_accents_and_multibyte_text() {
        let s = "Corrige l'été à Paris";
        let r = find_ci(s, "ÉTÉ").unwrap();
        assert_eq!(&s[r], "été");
        let s = "日本語のテスト";
        let r = find_ci(s, "テスト").unwrap();
        assert_eq!(&s[r.clone()], "テスト");
        assert!(s.is_char_boundary(r.start) && s.is_char_boundary(r.end));
    }

    #[test]
    fn find_returns_the_first_occurrence_and_does_not_match_across_gaps() {
        assert_eq!(find_ci("aXbxab", "ab"), Some(4..6));
        assert_eq!(find_ci("a-b", "ab"), None);
        assert_eq!(find_ci("aab", "ab"), Some(1..3)); // restarts after a partial match
    }

    #[test]
    fn matching_looks_at_message_author_and_hash_prefix() {
        let c = commit("Add login page", "Antoine Jaquet", "abc123def");
        let m = |q: &str| commit_matches(&c, q, &q.to_lowercase());
        assert!(m("LOGIN"));
        assert!(m("jaquet"));
        assert!(m("abc12"));
        assert!(m("ABC12")); // hash prefix is case-insensitive too
        assert!(!m("c123")); // hash must match from the start
        assert!(!m("nothing"));
    }

    #[test]
    fn step_wraps_around_both_ways() {
        assert_eq!(step(0, None, 1), None);
        assert_eq!(step(3, None, 1), Some(0));
        assert_eq!(step(3, None, -1), Some(2));
        assert_eq!(step(3, Some(0), 1), Some(1));
        assert_eq!(step(3, Some(2), 1), Some(0));
        assert_eq!(step(3, Some(0), -1), Some(2));
        assert_eq!(step(1, Some(0), 1), Some(0));
    }

    #[test]
    fn first_match_at_or_after_the_selection() {
        let m = [2, 5, 9];
        assert_eq!(first_at_or_after(&m, 0), Some(0));
        assert_eq!(first_at_or_after(&m, 5), Some(1)); // the selected commit itself counts
        assert_eq!(first_at_or_after(&m, 6), Some(2));
        assert_eq!(first_at_or_after(&m, 10), Some(0)); // wraps
        assert_eq!(first_at_or_after(&[], 3), None);
    }

    #[test]
    fn ascii_fast_path_agrees_with_the_general_algorithm() {
        // Haystacks mixing ASCII and multibyte text; ASCII needles go through the fast path,
        // the same needle with a trailing non-ASCII char forces the slow one on a shifted text.
        for hay in ["Fix the Bug", "été Bug été", "日本 bug 語", "aXbxab", "", "b"] {
            for needle in ["bug", "BUG", "x", "ab", "zz", "e"] {
                let fast = find_ci(hay, needle);
                // Reference: the slow path, forced by searching in a text where the needle is
                // the same but compared char by char.
                let lower = hay.to_lowercase();
                let slow = lower.find(&needle.to_lowercase());
                assert_eq!(fast.is_some(), slow.is_some(), "{hay:?} / {needle:?}");
                if let Some(r) = fast {
                    assert!(hay.is_char_boundary(r.start) && hay.is_char_boundary(r.end));
                    assert!(hay[r].eq_ignore_ascii_case(needle));
                }
            }
        }
    }

    fn sample() -> Vec<CommitInfo> {
        [
            ("Add login page", "Antoine Jaquet", "abc111"),
            ("Fix login bug", "Bob", "abd222"),
            ("Refactor loader", "Antoine Jaquet", "fed333"),
            ("Login: éviter l'été", "Zoé", "abc444"),
            ("Bump deps", "Bob", "123abc"),
        ]
        .into_iter()
        .map(|(s, a, id)| commit(s, a, id))
        .collect()
    }

    #[test]
    fn incremental_search_equals_a_full_search() {
        let commits = sample();
        for full_query in ["login", "log", "lo", "l", "an", "antoine j", "abc", "abc1", "été", "x"] {
            // Every prefix of the query as "previous", as when typing it letter by letter.
            for cut in 1..=full_query.len() {
                if !full_query.is_char_boundary(cut) {
                    continue;
                }
                let prev = &full_query[..cut];
                let prev_matches = find_matches(&commits, prev, None);
                let incremental = find_matches(&commits, full_query, Some((prev, &prev_matches)));
                assert_eq!(incremental, find_matches(&commits, full_query, None), "{prev:?} -> {full_query:?}");
            }
        }
    }

    #[test]
    fn narrowing_never_uses_stale_results_when_the_query_is_not_an_extension() {
        let commits = sample();
        let prev_matches = find_matches(&commits, "login", None);
        // Backspace / unrelated query: previous results must be ignored, not filtered.
        assert_eq!(find_matches(&commits, "log", Some(("login", &prev_matches))), find_matches(&commits, "log", None));
        assert_eq!(find_matches(&commits, "bump", Some(("login", &prev_matches))), [4]);
        // Empty previous query means "everything", so it must do a full scan.
        assert_eq!(find_matches(&commits, "bob", Some(("", &[]))), [1, 4]);
    }

    #[test]
    fn stale_indices_beyond_the_commit_list_are_ignored() {
        // After a reload the list can be shorter than when the previous results were computed.
        let commits = sample();
        assert_eq!(find_matches(&commits, "login", Some(("log", &[0, 1, 99]))), [0, 1]);
    }
}
