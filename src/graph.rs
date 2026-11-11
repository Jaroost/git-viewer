//! Computes the lane layout of a commit graph.
//!
//! Input: commits in topological order (children before parents, newest first).
//! Output: for each commit, which column it sits in and which line segments
//! must be drawn in its row. Pure logic, no GUI, so it is easy to test.

use crate::repo::CommitInfo;

/// A line from the top edge of the row, at `from`, to the bottom edge, at `to`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pass {
    pub from: usize,
    pub to: usize,
    pub color: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Row {
    /// Column of the commit node.
    pub col: usize,
    pub color: usize,
    /// Lines coming from the top edge (column, color) and ending at the node.
    pub incoming: Vec<(usize, usize)>,
    /// Lines that go straight through the row without touching the node.
    pub passing: Vec<Pass>,
    /// Lines leaving the node toward the bottom edge (column, color), one per parent.
    pub outgoing: Vec<(usize, usize)>,
}

#[derive(Debug, Default)]
pub struct Layout {
    pub rows: Vec<Row>,
    /// Number of columns needed to draw the whole graph.
    pub width: usize,
}

pub fn layout(commits: &[CommitInfo]) -> Layout {
    // Each lane waits for a given commit id and has a color.
    let mut lanes: Vec<Option<(String, usize)>> = Vec::new();
    let mut next_color = 0usize;
    let mut new_color = || {
        let c = next_color;
        next_color += 1;
        c
    };
    let mut rows = Vec::with_capacity(commits.len());
    let mut width = 0;

    for c in commits {
        let matching: Vec<usize> = lanes
            .iter()
            .enumerate()
            .filter(|(_, l)| l.as_ref().is_some_and(|(id, _)| *id == c.id))
            .map(|(i, _)| i)
            .collect();

        let col = matching.first().copied().unwrap_or_else(|| free_slot(&mut lanes));
        let color = match matching.first() {
            Some(&i) => lanes[i].as_ref().unwrap().1,
            None => new_color(),
        };

        let incoming: Vec<(usize, usize)> =
            matching.iter().map(|&i| (i, lanes[i].as_ref().unwrap().1)).collect();

        let passing: Vec<Pass> = lanes
            .iter()
            .enumerate()
            .filter(|(i, _)| !matching.contains(i))
            .filter_map(|(i, l)| l.as_ref().map(|(_, color)| Pass { from: i, to: i, color: *color }))
            .collect();

        for &i in &matching {
            lanes[i] = None;
        }
        // Make sure `col` exists in the vector even for a brand-new tip.
        if col >= lanes.len() {
            lanes.resize(col + 1, None);
        }

        let mut outgoing = Vec::new();
        for (n, p) in c.parents.iter().enumerate() {
            let existing = lanes
                .iter()
                .position(|l| l.as_ref().is_some_and(|(id, _)| id == p));
            match existing {
                Some(i) => outgoing.push((i, lanes[i].as_ref().unwrap().1)),
                None => {
                    // The first parent continues in the commit's own column.
                    let (slot, lane_color) = if n == 0 {
                        (col, color)
                    } else {
                        (free_slot(&mut lanes), new_color())
                    };
                    lanes[slot] = Some((p.clone(), lane_color));
                    outgoing.push((slot, lane_color));
                }
            }
        }

        while lanes.last().is_some_and(|l| l.is_none()) {
            lanes.pop();
        }

        let row_width = [col + 1]
            .into_iter()
            .chain(incoming.iter().map(|(i, _)| i + 1))
            .chain(passing.iter().map(|p| p.from.max(p.to) + 1))
            .chain(outgoing.iter().map(|(i, _)| i + 1))
            .max()
            .unwrap_or(1);
        width = width.max(row_width);

        rows.push(Row { col, color, incoming, passing, outgoing });
    }

    Layout { rows, width }
}

/// Returns the index of the first free lane, adding one if all are used.
fn free_slot(lanes: &mut Vec<Option<(String, usize)>>) -> usize {
    match lanes.iter().position(|l| l.is_none()) {
        Some(i) => i,
        None => {
            lanes.push(None);
            lanes.len() - 1
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn c(id: &str, parents: &[&str]) -> CommitInfo {
        CommitInfo {
            id: id.into(),
            summary: String::new(),
            author: String::new(),
            time: 0,
            parents: parents.iter().map(|s| s.to_string()).collect(),
        }
    }

    #[test]
    fn empty_input_gives_empty_layout() {
        let l = layout(&[]);
        assert!(l.rows.is_empty());
        assert_eq!(l.width, 0);
    }

    #[test]
    fn linear_history_stays_in_column_zero() {
        let l = layout(&[c("c", &["b"]), c("b", &["a"]), c("a", &[])]);
        assert_eq!(l.width, 1);
        assert!(l.rows.iter().all(|r| r.col == 0));
        assert!(l.rows.iter().all(|r| r.passing.is_empty()));
        assert!(l.rows[0].incoming.is_empty()); // tip: nothing above
        assert_eq!(l.rows[1].incoming.len(), 1);
        assert_eq!(l.rows[0].outgoing.len(), 1);
        assert!(l.rows[2].outgoing.is_empty()); // root: nothing below
        // The color is kept along the whole lane.
        assert_eq!(l.rows[0].color, l.rows[2].color);
    }

    #[test]
    fn fork_and_merge_use_two_columns() {
        // merge -> (main, feat) -> base
        let l = layout(&[
            c("merge", &["main", "feat"]),
            c("main", &["base"]),
            c("feat", &["base"]),
            c("base", &[]),
        ]);
        assert_eq!(l.width, 2);
        // merge has two outgoing lines in distinct columns with distinct colors
        let out = &l.rows[0].outgoing;
        assert_eq!(out.len(), 2);
        assert_ne!(out[0].0, out[1].0);
        assert_ne!(out[0].1, out[1].1);
        // main stays in col 0, feat in col 1
        assert_eq!(l.rows[1].col, 0);
        assert_eq!(l.rows[2].col, 1);
        // while main is drawn, the feat lane passes through
        assert_eq!(l.rows[1].passing, [Pass { from: 1, to: 1, color: out[1].1 }]);
        // feat's line bends into the existing lane of base (column 0)...
        assert_eq!(l.rows[2].outgoing, [(0, l.rows[1].color)]);
        // ...so base has a single incoming line and the feat lane is closed.
        assert_eq!(l.rows[3].col, 0);
        assert_eq!(l.rows[3].incoming.len(), 1);
        assert!(l.rows[3].passing.is_empty());
    }

    #[test]
    fn two_branch_tips_get_separate_columns() {
        // Two tips, both children of base.
        let l = layout(&[c("t1", &["base"]), c("t2", &["base"]), c("base", &[])]);
        assert_eq!(l.rows[0].col, 0);
        assert_eq!(l.rows[1].col, 1);
        assert_eq!(l.rows[2].col, 0);
        // The second tip's line to base joins the existing lane (column 0).
        assert_eq!(l.rows[1].outgoing, [(0, l.rows[0].color)]);
        assert_eq!(l.rows[2].incoming.len(), 1);
    }

    #[test]
    fn freed_columns_are_reused() {
        // feat merges back, then a new tip appears and should reuse column 1.
        let l = layout(&[
            c("merge", &["main", "feat"]),
            c("main", &["base"]),
            c("feat", &["base"]),
            c("other", &["base"]),
            c("base", &[]),
        ]);
        assert_eq!(l.rows[2].col, 1);
        assert_eq!(l.width, 2);
    }

    /// Manual benchmark on a real repo:
    /// `GV_BENCH_REPO=/path cargo test --release bench -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn bench_large_repository() {
        use crate::repo::Repo;
        use std::time::Instant;
        let path = std::env::var("GV_BENCH_REPO").expect("set GV_BENCH_REPO");
        let t = Instant::now();
        let repo = Repo::open(std::path::Path::new(&path)).unwrap();
        println!("open:                {:?}", t.elapsed());
        let t = Instant::now();
        let b = repo.branches().unwrap();
        println!("branches ({}):     {:?}", b.len(), t.elapsed());
        for limit in [2_000, 20_000, 200_000] {
            let t = Instant::now();
            let commits = repo.commits(limit).unwrap();
            let load = t.elapsed();
            let t = Instant::now();
            let l = layout(&commits);
            println!(
                "commits({limit}): load {load:?}, layout {:?}, width {}",
                t.elapsed(),
                l.width
            );
        }
        let all = repo.commits(200_000).unwrap();
        for q in ["commit 1", "merge b1", "zzz-no-match", "T"] {
            let q_lc = q.to_lowercase();
            let t = Instant::now();
            let n = all.iter().filter(|c| crate::search::commit_matches(c, q, &q_lc)).count();
            println!("search {q:?}: {n} matches in {:?}", t.elapsed());
        }
        let commits = repo.commits(2_000).unwrap();
        let t = Instant::now();
        let d = repo.commit_detail(&commits[0].id).unwrap();
        println!("detail: {:?} ({} files)", t.elapsed(), d.files.len());
    }
}
