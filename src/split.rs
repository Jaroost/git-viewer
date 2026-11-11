//! Side-by-side comparison: turns the lines of a diff into aligned left/right rows.

use crate::repo::DiffLine;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CellKind {
    Context,
    Removed,
    Added,
}

/// One side of a row. `line` indexes the diff's line list (to get the text and its colors).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cell {
    /// Line number in that version of the file (1-based).
    pub number: usize,
    pub line: usize,
    pub kind: CellKind,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SplitRow {
    /// Left = previous version, right = new version. A side is `None` where the other has
    /// lines that do not exist on it (pure additions / removals).
    Pair { left: Option<Cell>, right: Option<Cell> },
    /// Between two hunks that are not adjacent.
    Separator,
}

/// `"@@ -10,3 +12,4 @@"` → `(10, 12)`: the first line number of each side. A count of 1 may be omitted.
pub fn parse_hunk_header(header: &str) -> Option<(usize, usize)> {
    let rest = header.strip_prefix("@@ -")?;
    let (old, rest) = rest.split_once(" +")?;
    let new = rest.split_once(" @@").map_or(rest, |(n, _)| n);
    let start = |range: &str| range.split(',').next()?.parse::<usize>().ok();
    Some((start(old)?, start(new)?))
}

/// Aligns the lines of a diff: unchanged lines face each other, and a run of removed lines is
/// paired with the run of added lines that follows it, row by row; the shorter side is padded.
pub fn split_rows(lines: &[DiffLine]) -> Vec<SplitRow> {
    let mut rows = Vec::new();
    let (mut old_no, mut new_no) = (1usize, 1usize);
    let mut seen_hunk = false;
    let mut i = 0;
    while i < lines.len() {
        match lines[i].origin {
            'H' => {
                if let Some((o, n)) = parse_hunk_header(&lines[i].text) {
                    // For an empty side git writes "-0,0" / "+0,0": numbering then starts at 1.
                    (old_no, new_no) = (o.max(1), n.max(1));
                }
                if seen_hunk {
                    rows.push(SplitRow::Separator);
                }
                seen_hunk = true;
                i += 1;
            }
            ' ' => {
                rows.push(SplitRow::Pair {
                    left: Some(Cell { number: old_no, line: i, kind: CellKind::Context }),
                    right: Some(Cell { number: new_no, line: i, kind: CellKind::Context }),
                });
                old_no += 1;
                new_no += 1;
                i += 1;
            }
            _ => {
                let removed_start = i;
                while i < lines.len() && lines[i].origin == '-' {
                    i += 1;
                }
                let added_start = i;
                while i < lines.len() && lines[i].origin == '+' {
                    i += 1;
                }
                let (removed, added) = (removed_start..added_start, added_start..i);
                for k in 0..removed.len().max(added.len()) {
                    let left = (k < removed.len()).then(|| {
                        let c = Cell { number: old_no, line: removed.start + k, kind: CellKind::Removed };
                        old_no += 1;
                        c
                    });
                    let right = (k < added.len()).then(|| {
                        let c = Cell { number: new_no, line: added.start + k, kind: CellKind::Added };
                        new_no += 1;
                        c
                    });
                    rows.push(SplitRow::Pair { left, right });
                }
                if removed.is_empty() && added.is_empty() {
                    i += 1; // unknown origin: never loop forever
                }
            }
        }
    }
    rows
}

/// Row to scroll to so the first change is visible, with a little context above it.
pub fn first_change_row(rows: &[SplitRow]) -> Option<usize> {
    let first = rows.iter().position(|r| match r {
        SplitRow::Pair { left, right } => {
            left.is_some_and(|c| c.kind != CellKind::Context) || right.is_some_and(|c| c.kind != CellKind::Context)
        }
        SplitRow::Separator => false,
    })?;
    Some(first.saturating_sub(3))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Builds diff lines from a compact description: `H<header>`, ` text`, `-text`, `+text`.
    fn lines(spec: &[&str]) -> Vec<DiffLine> {
        spec.iter()
            .map(|s| {
                let (o, t) = s.split_at(1);
                DiffLine { origin: o.chars().next().unwrap(), text: t.to_string() }
            })
            .collect()
    }

    fn pair(l: Option<(usize, usize, CellKind)>, r: Option<(usize, usize, CellKind)>) -> SplitRow {
        let cell = |c: (usize, usize, CellKind)| Cell { number: c.0, line: c.1, kind: c.2 };
        SplitRow::Pair { left: l.map(cell), right: r.map(cell) }
    }

    #[test]
    fn hunk_headers_are_parsed() {
        assert_eq!(parse_hunk_header("@@ -10,3 +12,4 @@"), Some((10, 12)));
        assert_eq!(parse_hunk_header("@@ -1 +1 @@ fn main()"), Some((1, 1)));
        assert_eq!(parse_hunk_header("@@ -0,0 +1,3 @@"), Some((0, 1)));
        assert_eq!(parse_hunk_header("@@ -5,2 +0,0 @@"), Some((5, 0)));
        assert_eq!(parse_hunk_header("not a header"), None);
        assert_eq!(parse_hunk_header("@@ -x +1 @@"), None);
        assert_eq!(parse_hunk_header(""), None);
    }

    #[test]
    fn unchanged_lines_face_each_other_with_their_own_numbers() {
        use CellKind::Context as C;
        let rows = split_rows(&lines(&["H@@ -10,2 +12,2 @@", " a", " b"]));
        assert_eq!(rows, [pair(Some((10, 1, C)), Some((12, 1, C))), pair(Some((11, 2, C)), Some((13, 2, C)))]);
    }

    #[test]
    fn a_modified_line_pairs_the_removed_with_the_added_one() {
        use CellKind::*;
        let rows = split_rows(&lines(&["H@@ -1,3 +1,3 @@", " keep", "-old", "+new", " end"]));
        assert_eq!(
            rows,
            [
                pair(Some((1, 1, Context)), Some((1, 1, Context))),
                pair(Some((2, 2, Removed)), Some((2, 3, Added))),
                pair(Some((3, 4, Context)), Some((3, 4, Context))),
            ]
        );
    }

    #[test]
    fn unequal_runs_are_padded_on_the_shorter_side_and_numbering_stays_right() {
        use CellKind::*;
        // 3 lines replaced by 1, then 1 replaced by 2, then a context line.
        let rows = split_rows(&lines(&["H@@ -1,5 +1,4 @@", "-a", "-b", "-c", "+X", " mid", "-d", "+Y", "+Z", " end"]));
        assert_eq!(
            rows,
            [
                pair(Some((1, 1, Removed)), Some((1, 4, Added))),
                pair(Some((2, 2, Removed)), None),
                pair(Some((3, 3, Removed)), None),
                pair(Some((4, 5, Context)), Some((2, 5, Context))),
                pair(Some((5, 6, Removed)), Some((3, 7, Added))),
                pair(None, Some((4, 8, Added))),
                pair(Some((6, 9, Context)), Some((5, 9, Context))),
            ]
        );
    }

    #[test]
    fn pure_additions_leave_the_left_side_empty() {
        use CellKind::*;
        let rows = split_rows(&lines(&["H@@ -0,0 +1,2 @@", "+x", "+y"]));
        assert_eq!(rows, [pair(None, Some((1, 1, Added))), pair(None, Some((2, 2, Added)))]);
    }

    #[test]
    fn pure_deletions_leave_the_right_side_empty() {
        use CellKind::*;
        let rows = split_rows(&lines(&["H@@ -1,2 +0,0 @@", "-x", "-y"]));
        assert_eq!(rows, [pair(Some((1, 1, Removed)), None), pair(Some((2, 2, Removed)), None)]);
    }

    #[test]
    fn several_hunks_are_separated_and_renumbered() {
        use CellKind::Context as C;
        let rows = split_rows(&lines(&["H@@ -1,1 +1,1 @@", " a", "H@@ -50,1 +60,1 @@", " z"]));
        assert_eq!(
            rows,
            [pair(Some((1, 1, C)), Some((1, 1, C))), SplitRow::Separator, pair(Some((50, 3, C)), Some((60, 3, C)))]
        );
    }

    #[test]
    fn empty_input_and_unparsable_headers_do_not_break_anything() {
        assert!(split_rows(&[]).is_empty());
        // No usable header: numbering simply starts at 1 on both sides.
        let rows = split_rows(&lines(&["Hgarbage", " a"]));
        assert_eq!(rows.len(), 1);
    }

    #[test]
    fn every_diff_line_appears_exactly_once() {
        let l = lines(&["H@@ -1,6 +1,6 @@", " a", "-b", "-c", "+B", " d", "+e", "-f", " g"]);
        let rows = split_rows(&l);
        let mut seen: Vec<usize> = rows
            .iter()
            .flat_map(|r| match r {
                SplitRow::Pair { left, right } => {
                    let mut v = vec![];
                    // A context line is the same diff line on both sides: count it once.
                    if let Some(c) = left {
                        v.push(c.line);
                    }
                    if let Some(c) = right.filter(|c| c.kind != CellKind::Context) {
                        v.push(c.line);
                    }
                    v
                }
                SplitRow::Separator => vec![],
            })
            .collect();
        seen.sort_unstable();
        assert_eq!(seen, (1..l.len()).collect::<Vec<_>>());
    }

    #[test]
    fn first_change_row_leaves_context_above_it() {
        let l = lines(&["H@@ -1,8 +1,8 @@", " 1", " 2", " 3", " 4", " 5", "-6", "+6b", " 7"]);
        let rows = split_rows(&l);
        assert_eq!(first_change_row(&rows), Some(2)); // change at row 5, 3 rows of context above
        assert_eq!(first_change_row(&split_rows(&lines(&["H@@ -1 +1 @@", " a"]))), None);
        assert_eq!(first_change_row(&[]), None);
    }
}
