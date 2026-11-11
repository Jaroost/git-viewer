use anyhow::Result;
use git2::{BranchType, Delta, DiffOptions, Oid, Patch, Repository, Sort};
use std::path::{Path, PathBuf};

/// Maximum number of diff lines kept per commit, to keep huge commits responsive.
const MAX_DIFF_LINES: usize = 20_000;

pub struct CommitInfo {
    pub id: String,
    pub summary: String,
    pub author: String,
    pub time: i64,
    pub parents: Vec<String>,
}

pub struct BranchInfo {
    pub name: String,
    pub is_head: bool,
    /// Id of the commit the branch points to.
    pub tip: String,
    /// The branch tracks a remote branch (i.e. it has been published).
    pub has_upstream: bool,
}

/// A remote-tracking branch such as `origin/main` (`remote` = "origin", `name` = "main").
pub struct RemoteBranchInfo {
    pub remote: String,
    pub name: String,
    pub tip: String,
}

pub struct TagInfo {
    pub name: String,
    /// Id of the commit the tag points to (annotated tags are peeled).
    pub tip: String,
}

pub struct DiffLine {
    /// '+', '-', ' ' for content lines, 'H' for a hunk header.
    pub origin: char,
    pub text: String,
}

pub struct FileDiff {
    /// Unique per computed diff: lets the UI cache work (e.g. highlighting) done for it.
    pub uid: u64,
    pub path: String,
    /// Previous path when the file was renamed or copied.
    pub old_path: Option<String>,
    /// 'A'dded, 'D'eleted, 'M'odified, 'R'enamed, 'C'opied, '?' other.
    pub status: char,
    pub additions: usize,
    pub deletions: usize,
    pub binary: bool,
    pub lines: Vec<DiffLine>,
}

pub struct CommitDetail {
    pub id: String,
    pub message: String,
    pub author: String,
    pub email: String,
    pub time: i64,
    pub parents: Vec<String>,
    pub files: Vec<FileDiff>,
    /// True if the diff was cut at `MAX_DIFF_LINES`.
    pub truncated: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefKind {
    Branch,
    Tag,
}

/// A branch or tag pointing at a commit. A local branch and its remote counterparts
/// (`main`, `origin/main`) pointing at the same commit are merged into one badge.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RefBadge {
    /// Short name, without the remote prefix (`main`, not `origin/main`).
    pub name: String,
    pub kind: RefKind,
    /// A local branch with this name exists here.
    pub local: bool,
    /// Remotes (e.g. `origin`) that have this branch here.
    pub remotes: Vec<String>,
    /// HEAD is on this local branch.
    pub is_head: bool,
}

impl RefBadge {
    pub fn has_remote(&self) -> bool {
        !self.remotes.is_empty()
    }
}

pub struct Repo {
    pub(crate) inner: Repository,
}

impl Repo {
    pub fn open(path: &Path) -> Result<Self> {
        Ok(Self { inner: Repository::discover(path)? })
    }

    /// Root folder of the working tree (or the git dir for bare repositories).
    pub fn root(&self) -> PathBuf {
        let p = self.inner.workdir().unwrap_or_else(|| self.inner.path());
        dunce::canonicalize(p).unwrap_or_else(|_| p.to_path_buf())
    }

    /// Full message, metadata and diff against the first parent (or the empty tree for a root commit).
    pub fn commit_detail(&self, id: &str) -> Result<CommitDetail> {
        let commit = self.inner.find_commit(Oid::from_str(id)?)?;
        let tree = commit.tree()?;
        let parent_tree = match commit.parent_count() {
            0 => None,
            _ => Some(commit.parent(0)?.tree()?),
        };
        let mut diff = self.inner.diff_tree_to_tree(
            parent_tree.as_ref(),
            Some(&tree),
            Some(&mut DiffOptions::new()),
        )?;
        diff.find_similar(None)?;

        let (files, truncated) = files_from_diff(&diff)?;

        let author = commit.author();
        Ok(CommitDetail {
            id: commit.id().to_string(),
            message: commit.message().unwrap_or("").trim_end().to_string(),
            author: author.name().unwrap_or("?").to_string(),
            email: author.email().unwrap_or("").to_string(),
            time: commit.time().seconds(),
            parents: commit.parent_ids().map(|p| p.to_string()).collect(),
            files,
            truncated,
        })
    }

    /// The whole file as one hunk: every line of it, with the changed ones marked, instead of the
    /// changed lines plus a few lines of context. `old_path` is the previous name for renames.
    /// The boolean tells whether the file was too long and got cut.
    pub fn file_diff_full(&self, id: &str, path: &str, old_path: Option<&str>) -> Result<(FileDiff, bool)> {
        let commit = self.inner.find_commit(Oid::from_str(id)?)?;
        let tree = commit.tree()?;
        let parent_tree = match commit.parent_count() {
            0 => None,
            _ => Some(commit.parent(0)?.tree()?),
        };
        let mut opts = DiffOptions::new();
        opts.context_lines(10_000_000).disable_pathspec_match(true).pathspec(path);
        if let Some(old) = old_path {
            opts.pathspec(old); // keep both sides of a rename so it is detected as one
        }
        let mut diff = self.inner.diff_tree_to_tree(parent_tree.as_ref(), Some(&tree), Some(&mut opts))?;
        diff.find_similar(None)?;
        let (files, truncated) = files_from_diff(&diff)?;
        let file = files
            .into_iter()
            .find(|f| f.path == path)
            .ok_or_else(|| anyhow::anyhow!("« {path} » n'est pas modifié par ce commit"))?;
        Ok((file, truncated))
    }

    /// Branches and tags grouped by the id of the commit they point to.
    /// Local and remote versions of the same branch on the same commit share one badge.
    pub fn refs(&self) -> Result<std::collections::HashMap<String, Vec<RefBadge>>> {
        let head = self.inner.head().ok().and_then(|h| h.name().ok().map(str::to_string));
        let mut map: std::collections::HashMap<String, Vec<RefBadge>> = Default::default();
        for r in self.inner.references()? {
            let r = r?;
            let Ok(full) = r.name() else { continue };
            // (kind, short name, remote name if it is a remote-tracking branch)
            let (kind, name, remote) = if let Some(n) = full.strip_prefix("refs/heads/") {
                (RefKind::Branch, n, None)
            } else if let Some(n) = full.strip_prefix("refs/remotes/") {
                match n.split_once('/') {
                    Some((_, "HEAD")) | None => continue,
                    Some((remote, short)) => (RefKind::Branch, short, Some(remote)),
                }
            } else if let Some(n) = full.strip_prefix("refs/tags/") {
                (RefKind::Tag, n, None)
            } else {
                continue;
            };
            let Ok(commit) = r.peel_to_commit() else { continue };
            let badges = map.entry(commit.id().to_string()).or_default();
            let idx = match badges.iter().position(|b| b.kind == kind && b.name == name) {
                Some(i) => i,
                None => {
                    badges.push(RefBadge {
                        name: name.to_string(),
                        kind,
                        local: false,
                        remotes: Vec::new(),
                        is_head: false,
                    });
                    badges.len() - 1
                }
            };
            let badge = &mut badges[idx];
            match remote {
                Some(remote) => badge.remotes.push(remote.to_string()),
                None if kind == RefKind::Branch => {
                    badge.local = true;
                    badge.is_head = head.as_deref() == Some(full);
                }
                None => {}
            }
        }
        for badges in map.values_mut() {
            for b in badges.iter_mut() {
                b.remotes.sort();
            }
            badges.sort_by(|a, b| b.is_head.cmp(&a.is_head).then(a.name.cmp(&b.name)));
        }
        Ok(map)
    }

    pub fn commits(&self, limit: usize) -> Result<Vec<CommitInfo>> {
        let mut walk = self.inner.revwalk()?;
        walk.set_sorting(Sort::TOPOLOGICAL | Sort::TIME)?;
        walk.push_glob("refs/heads/*")?;
        walk.push_glob("refs/remotes/*")?;
        let _ = walk.push_head(); // detached HEAD; errors on an empty repository
        let mut out = Vec::new();
        for oid in walk.take(limit) {
            let c = self.inner.find_commit(oid?)?;
            let id = c.id().to_string();
            out.push(CommitInfo {
                id,
                summary: c.summary()?.unwrap_or("").to_string(),
                author: c.author().name().unwrap_or("?").to_string(),
                time: c.time().seconds(),
                parents: c.parent_ids().map(|p| p.to_string()).collect(),
            });
        }
        Ok(out)
    }

    /// Local branches, sorted by name (case-insensitive).
    pub fn branches(&self) -> Result<Vec<BranchInfo>> {
        let mut out = Vec::new();
        for b in self.inner.branches(Some(BranchType::Local))? {
            let (b, _) = b?;
            let Ok(tip) = b.get().peel_to_commit() else { continue };
            out.push(BranchInfo {
                name: b.name()?.unwrap_or("?").to_string(),
                is_head: b.is_head(),
                tip: tip.id().to_string(),
                has_upstream: b.upstream().is_ok(),
            });
        }
        out.sort_by_key(|b| b.name.to_lowercase());
        Ok(out)
    }

    /// Remote-tracking branches (`origin/main`…), sorted by remote then name. `*/HEAD` is skipped.
    pub fn remote_branches(&self) -> Result<Vec<RemoteBranchInfo>> {
        let mut out = Vec::new();
        for b in self.inner.branches(Some(BranchType::Remote))? {
            let (b, _) = b?;
            let Some(full) = b.name()? else { continue };
            let Some((remote, name)) = full.split_once('/') else { continue };
            if name == "HEAD" {
                continue;
            }
            let Ok(tip) = b.get().peel_to_commit() else { continue };
            out.push(RemoteBranchInfo {
                remote: remote.to_string(),
                name: name.to_string(),
                tip: tip.id().to_string(),
            });
        }
        out.sort_by_key(|b| (b.remote.to_lowercase(), b.name.to_lowercase()));
        Ok(out)
    }

    /// Tags, sorted by name (case-insensitive). Tags that do not point to a commit are skipped.
    pub fn tags(&self) -> Result<Vec<TagInfo>> {
        let mut out = Vec::new();
        for r in self.inner.references_glob("refs/tags/*")? {
            let r = r?;
            let Ok(full) = r.name() else { continue };
            let Ok(commit) = r.peel_to_commit() else { continue };
            out.push(TagInfo {
                name: full.trim_start_matches("refs/tags/").to_string(),
                tip: commit.id().to_string(),
            });
        }
        out.sort_by_key(|t| t.name.to_lowercase());
        Ok(out)
    }
}

fn next_uid() -> u64 {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
}

/// Converts a libgit2 diff into per-file line lists, capped at `MAX_DIFF_LINES` lines overall.
pub(crate) fn files_from_diff(diff: &git2::Diff<'_>) -> Result<(Vec<FileDiff>, bool)> {
    let mut files = Vec::new();
    let mut budget = MAX_DIFF_LINES;
    let mut truncated = false;
    for idx in 0..diff.deltas().len() {
        let delta = diff.get_delta(idx).expect("index within deltas().len()");
        let path = delta
            .new_file()
            .path()
            .or_else(|| delta.old_file().path())
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_default();
        let status = match delta.status() {
            Delta::Added => 'A',
            Delta::Deleted => 'D',
            Delta::Modified => 'M',
            Delta::Renamed => 'R',
            Delta::Copied => 'C',
            _ => '?',
        };
        let old_path = delta
            .old_file()
            .path()
            .map(|p| p.to_string_lossy().into_owned())
            .filter(|old| *old != path);
        let mut file = FileDiff {
            uid: next_uid(),
            path,
            old_path,
            status,
            additions: 0,
            deletions: 0,
            binary: false,
            lines: Vec::new(),
        };
        if let Some(patch) = Patch::from_diff(diff, idx)? {
            // The binary flag is only set once the patch has been generated.
            file.binary = patch.delta().flags().is_binary();
            let (_, adds, dels) = patch.line_stats()?;
            file.additions = adds;
            file.deletions = dels;
            'hunks: for h in 0..patch.num_hunks() {
                let (hunk, n_lines) = patch.hunk(h)?;
                if budget == 0 {
                    truncated = true;
                    break;
                }
                budget -= 1;
                file.lines.push(DiffLine {
                    origin: 'H',
                    text: String::from_utf8_lossy(hunk.header()).trim_end().to_string(),
                });
                for l in 0..n_lines {
                    if budget == 0 {
                        truncated = true;
                        break 'hunks;
                    }
                    budget -= 1;
                    let line = patch.line_in_hunk(h, l)?;
                    let origin = line.origin();
                    if !matches!(origin, '+' | '-' | ' ') {
                        continue; // e.g. "\ No newline at end of file"
                    }
                    file.lines.push(DiffLine {
                        origin,
                        text: String::from_utf8_lossy(line.content())
                            .trim_end_matches(['\n', '\r'])
                            .to_string(),
                    });
                }
            }
        }
        files.push(file);
    }
    Ok((files, truncated))
}

#[cfg(test)]
mod tests {
    use super::*;
    use git2::{Oid, Signature};
    use tempfile::TempDir;

    fn init() -> (TempDir, Repository) {
        let dir = TempDir::new().unwrap();
        let repo = Repository::init(dir.path()).unwrap();
        (dir, repo)
    }

    /// Commit on HEAD (or on `refname` if given) with the given parents.
    fn commit(repo: &Repository, msg: &str, parents: &[Oid], refname: &str) -> Oid {
        let sig = Signature::now("Tester", "t@example.com").unwrap();
        let tree = repo.find_tree(repo.treebuilder(None).unwrap().write().unwrap()).unwrap();
        let parents: Vec<_> = parents.iter().map(|o| repo.find_commit(*o).unwrap()).collect();
        let refs: Vec<_> = parents.iter().collect();
        repo.commit(Some(refname), &sig, &sig, msg, &tree, &refs).unwrap()
    }

    #[test]
    fn open_fails_outside_a_repository() {
        let dir = TempDir::new().unwrap();
        assert!(Repo::open(dir.path()).is_err());
    }

    #[test]
    fn open_discovers_repo_from_subdirectory() {
        let (dir, _repo) = init();
        let sub = dir.path().join("a/b");
        std::fs::create_dir_all(&sub).unwrap();
        assert!(Repo::open(&sub).is_ok());
    }

    #[test]
    fn empty_repository_has_no_commits_and_no_branches() {
        let (dir, _repo) = init();
        let r = Repo::open(dir.path()).unwrap();
        assert!(r.commits(100).unwrap().is_empty());
        assert!(r.branches().unwrap().is_empty());
    }

    #[test]
    fn linear_history_is_newest_first_with_parents() {
        let (dir, repo) = init();
        let a = commit(&repo, "first", &[], "HEAD");
        let b = commit(&repo, "second", &[a], "HEAD");
        let c = commit(&repo, "third", &[b], "HEAD");

        let commits = Repo::open(dir.path()).unwrap().commits(100).unwrap();
        let summaries: Vec<_> = commits.iter().map(|c| c.summary.as_str()).collect();
        assert_eq!(summaries, ["third", "second", "first"]);
        assert_eq!(commits[0].id, c.to_string());
        assert_eq!(commits[0].parents, [b.to_string()]);
        assert!(commits[2].parents.is_empty());
        assert_eq!(commits[0].author, "Tester");
    }

    #[test]
    fn limit_truncates_the_history() {
        let (dir, repo) = init();
        let mut last = commit(&repo, "c0", &[], "HEAD");
        for i in 1..5 {
            last = commit(&repo, &format!("c{i}"), &[last], "HEAD");
        }
        let r = Repo::open(dir.path()).unwrap();
        assert_eq!(r.commits(2).unwrap().len(), 2);
        assert_eq!(r.commits(100).unwrap().len(), 5);
    }

    #[test]
    fn commits_from_all_local_branches_are_included() {
        let (dir, repo) = init();
        let base = commit(&repo, "base", &[], "HEAD");
        commit(&repo, "on-feature", &[base], "refs/heads/feature");

        let commits = Repo::open(dir.path()).unwrap().commits(100).unwrap();
        let summaries: Vec<_> = commits.iter().map(|c| c.summary.as_str()).collect();
        assert_eq!(commits.len(), 2);
        assert!(summaries.contains(&"base") && summaries.contains(&"on-feature"));
    }

    #[test]
    fn merge_commit_has_two_parents_and_children_precede_parents() {
        let (dir, repo) = init();
        let base = commit(&repo, "base", &[], "HEAD");
        let main = commit(&repo, "main-work", &[base], "HEAD");
        let feat = commit(&repo, "feat-work", &[base], "refs/heads/feature");
        let merge = commit(&repo, "merge", &[main, feat], "HEAD");

        let commits = Repo::open(dir.path()).unwrap().commits(100).unwrap();
        assert_eq!(commits.len(), 4);
        let pos = |id: Oid| commits.iter().position(|c| c.id == id.to_string()).unwrap();
        assert_eq!(commits[pos(merge)].parents.len(), 2);
        // Topological order: every commit appears before its parents.
        for c in &commits {
            for p in &c.parents {
                assert!(pos(c.id.parse().unwrap()) < pos(p.parse().unwrap()));
            }
        }
    }

    #[test]
    fn branches_report_which_one_is_head() {
        let (dir, repo) = init();
        let base = commit(&repo, "base", &[], "HEAD");
        commit(&repo, "x", &[base], "refs/heads/feature");

        let branches = Repo::open(dir.path()).unwrap().branches().unwrap();
        assert_eq!(branches.len(), 2);
        let head: Vec<_> = branches.iter().filter(|b| b.is_head).collect();
        assert_eq!(head.len(), 1);
        assert!(branches.iter().any(|b| b.name == "feature" && !b.is_head));
    }

    /// Commit on `refname` with a tree made of the given files.
    fn commit_files(
        repo: &Repository,
        msg: &str,
        parents: &[Oid],
        refname: &str,
        files: &[(&str, &str)],
    ) -> Oid {
        let sig = Signature::now("Tester", "t@example.com").unwrap();
        let mut tb = repo.treebuilder(None).unwrap();
        for (name, content) in files {
            let blob = repo.blob(content.as_bytes()).unwrap();
            tb.insert(name, blob, 0o100644).unwrap();
        }
        let tree = repo.find_tree(tb.write().unwrap()).unwrap();
        let parents: Vec<_> = parents.iter().map(|o| repo.find_commit(*o).unwrap()).collect();
        let refs: Vec<_> = parents.iter().collect();
        repo.commit(Some(refname), &sig, &sig, msg, &tree, &refs).unwrap()
    }

    #[test]
    fn root_is_the_working_tree_folder() {
        let (dir, _repo) = init();
        let sub = dir.path().join("x");
        std::fs::create_dir_all(&sub).unwrap();
        let r = Repo::open(&sub).unwrap();
        assert_eq!(r.root(), dunce::canonicalize(dir.path()).unwrap());
    }

    #[test]
    fn detail_of_root_commit_lists_added_files_with_lines() {
        let (dir, repo) = init();
        let id = commit_files(&repo, "add files\n\nlong body", &[], "HEAD", &[("a.txt", "one\ntwo\n"), ("b.txt", "x\n")]);
        let d = Repo::open(dir.path()).unwrap().commit_detail(&id.to_string()).unwrap();
        assert_eq!(d.message, "add files\n\nlong body");
        assert_eq!(d.author, "Tester");
        assert_eq!(d.email, "t@example.com");
        assert!(d.parents.is_empty());
        assert_eq!(d.files.len(), 2);
        let a = d.files.iter().find(|f| f.path == "a.txt").unwrap();
        assert_eq!((a.status, a.additions, a.deletions), ('A', 2, 0));
        let texts: Vec<_> = a.lines.iter().map(|l| (l.origin, l.text.as_str())).collect();
        assert_eq!(texts[1..], [('+', "one"), ('+', "two")]);
        assert_eq!(a.lines[0].origin, 'H');
    }

    #[test]
    fn detail_shows_modifications_and_deletions_against_first_parent() {
        let (dir, repo) = init();
        let c1 = commit_files(&repo, "c1", &[], "HEAD", &[("a.txt", "keep\nold\n"), ("gone.txt", "bye\n")]);
        let c2 = commit_files(&repo, "c2", &[c1], "HEAD", &[("a.txt", "keep\nnew\n")]);
        let d = Repo::open(dir.path()).unwrap().commit_detail(&c2.to_string()).unwrap();
        let a = d.files.iter().find(|f| f.path == "a.txt").unwrap();
        assert_eq!((a.status, a.additions, a.deletions), ('M', 1, 1));
        assert!(a.lines.iter().any(|l| l.origin == '-' && l.text == "old"));
        assert!(a.lines.iter().any(|l| l.origin == '+' && l.text == "new"));
        assert!(a.lines.iter().any(|l| l.origin == ' ' && l.text == "keep"));
        let g = d.files.iter().find(|f| f.path == "gone.txt").unwrap();
        assert_eq!((g.status, g.deletions), ('D', 1));
    }

    #[test]
    fn detail_marks_binary_files() {
        let (dir, repo) = init();
        let sig = Signature::now("Tester", "t@example.com").unwrap();
        let blob = repo.blob(&[0u8, 159, 146, 150, 0, 1, 2]).unwrap();
        let mut tb = repo.treebuilder(None).unwrap();
        tb.insert("img.bin", blob, 0o100644).unwrap();
        let tree = repo.find_tree(tb.write().unwrap()).unwrap();
        let id = repo.commit(Some("HEAD"), &sig, &sig, "bin", &tree, &[]).unwrap();
        let d = Repo::open(dir.path()).unwrap().commit_detail(&id.to_string()).unwrap();
        assert!(d.files[0].binary);
        assert!(d.files[0].lines.is_empty());
    }

    #[test]
    fn detail_rejects_unknown_ids() {
        let (dir, _repo) = init();
        let r = Repo::open(dir.path()).unwrap();
        assert!(r.commit_detail("not-an-id").is_err());
        assert!(r.commit_detail(&"0".repeat(40)).is_err());
    }

    #[test]
    fn refs_group_branches_and_tags_by_commit() {
        let (dir, repo) = init();
        let a = commit(&repo, "a", &[], "HEAD");
        let b = commit(&repo, "b", &[a], "refs/heads/feature");
        let obj = repo.find_object(a, None).unwrap();
        repo.tag_lightweight("v1", &obj, false).unwrap();
        let refs = Repo::open(dir.path()).unwrap().refs().unwrap();
        let on_a = &refs[&a.to_string()];
        assert!(on_a.iter().any(|r| r.kind == RefKind::Tag && r.name == "v1"));
        assert!(on_a.iter().any(|r| r.kind == RefKind::Branch && r.is_head));
        assert_eq!(refs[&b.to_string()][0].name, "feature");
        assert!(!refs[&b.to_string()][0].is_head);
    }

    #[test]
    fn local_and_remote_branch_on_same_commit_share_one_badge() {
        let (dir, repo) = init();
        let a = commit(&repo, "a", &[], "HEAD");
        repo.reference("refs/remotes/origin/master", a, true, "t").unwrap();
        repo.reference("refs/remotes/origin/HEAD", a, true, "t").unwrap();
        repo.reference("refs/remotes/upstream/master", a, true, "t").unwrap();
        // A remote-only branch on another commit.
        let b = commit(&repo, "b", &[a], "refs/remotes/origin/feature/x");
        let refs = Repo::open(dir.path()).unwrap().refs().unwrap();

        let on_a = &refs[&a.to_string()];
        assert_eq!(on_a.len(), 1, "{on_a:?}"); // origin/HEAD ignored, no duplicates
        assert_eq!(on_a[0].name, "master");
        assert!(on_a[0].local && on_a[0].is_head);
        assert_eq!(on_a[0].remotes, ["origin", "upstream"]);

        let on_b = &refs[&b.to_string()];
        assert_eq!(on_b.len(), 1);
        assert_eq!(on_b[0].name, "feature/x"); // only the first segment is the remote
        assert!(!on_b[0].local && on_b[0].has_remote());
    }

    #[test]
    fn local_branch_without_remote_has_no_remote_flag() {
        let (dir, repo) = init();
        let a = commit(&repo, "a", &[], "HEAD");
        let refs = Repo::open(dir.path()).unwrap().refs().unwrap();
        let b = &refs[&a.to_string()][0];
        assert!(b.local && !b.has_remote());
    }

    #[test]
    fn branches_report_tip_upstream_and_are_sorted() {
        let (dir, repo) = init();
        let a = commit(&repo, "a", &[], "HEAD");
        let b = commit(&repo, "b", &[a], "refs/heads/Zed");
        commit(&repo, "c", &[a], "refs/heads/alpha");
        repo.remote("origin", "https://example.com/x.git").unwrap();
        repo.reference("refs/remotes/origin/master", a, true, "t").unwrap();
        repo.find_branch("master", BranchType::Local).unwrap().set_upstream(Some("origin/master")).unwrap();

        let branches = Repo::open(dir.path()).unwrap().branches().unwrap();
        let names: Vec<_> = branches.iter().map(|b| b.name.as_str()).collect();
        assert_eq!(names, ["alpha", "master", "Zed"]); // case-insensitive order
        let master = branches.iter().find(|b| b.name == "master").unwrap();
        assert_eq!(master.tip, a.to_string());
        assert!(master.has_upstream);
        let zed = branches.iter().find(|b| b.name == "Zed").unwrap();
        assert_eq!(zed.tip, b.to_string());
        assert!(!zed.has_upstream);
    }

    #[test]
    fn remote_branches_are_split_into_remote_and_name() {
        let (dir, repo) = init();
        let a = commit(&repo, "a", &[], "HEAD");
        repo.reference("refs/remotes/origin/main", a, true, "t").unwrap();
        repo.reference("refs/remotes/origin/feature/x", a, true, "t").unwrap();
        repo.reference("refs/remotes/origin/HEAD", a, true, "t").unwrap();
        repo.reference("refs/remotes/fork/main", a, true, "t").unwrap();

        let rb = Repo::open(dir.path()).unwrap().remote_branches().unwrap();
        let pairs: Vec<_> = rb.iter().map(|b| (b.remote.as_str(), b.name.as_str())).collect();
        assert_eq!(pairs, [("fork", "main"), ("origin", "feature/x"), ("origin", "main")]);
        assert!(rb.iter().all(|b| b.tip == a.to_string()));
    }

    #[test]
    fn tags_include_annotated_ones_pointing_at_their_commit() {
        let (dir, repo) = init();
        let a = commit(&repo, "a", &[], "HEAD");
        let b = commit(&repo, "b", &[a], "HEAD");
        let obj_a = repo.find_object(a, None).unwrap();
        repo.tag_lightweight("v1", &obj_a, false).unwrap();
        let sig = Signature::now("Tester", "t@example.com").unwrap();
        let obj_b = repo.find_object(b, None).unwrap();
        repo.tag("Release", &obj_b, &sig, "annotated", false).unwrap();

        let tags = Repo::open(dir.path()).unwrap().tags().unwrap();
        let got: Vec<_> = tags.iter().map(|t| (t.name.as_str(), t.tip.clone())).collect();
        assert_eq!(got, [("Release", b.to_string()), ("v1", a.to_string())]);
    }

    #[test]
    fn every_computed_file_diff_gets_a_distinct_uid() {
        let (dir, repo) = init();
        let c1 = commit_files(&repo, "c1", &[], "HEAD", &[("a.txt", "1\n"), ("b.txt", "1\n")]);
        let r = Repo::open(dir.path()).unwrap();
        let first = r.commit_detail(&c1.to_string()).unwrap();
        let second = r.commit_detail(&c1.to_string()).unwrap();
        let uids: std::collections::HashSet<_> =
            first.files.iter().chain(second.files.iter()).map(|f| f.uid).collect();
        assert_eq!(uids.len(), 4); // same commit computed twice: still new ids, no stale cache hits
    }

    // ------------------------------------------------------- whole-file diff

    fn numbered(n: usize, changed: Option<(usize, &str)>) -> String {
        (1..=n)
            .map(|i| match changed {
                Some((at, text)) if at == i => format!("{text}\n"),
                _ => format!("line {i}\n"),
            })
            .collect()
    }

    #[test]
    fn full_diff_has_every_line_while_the_normal_diff_only_has_the_surroundings() {
        let (dir, repo) = init();
        let c1 = commit_files(&repo, "c1", &[], "HEAD", &[("f.txt", &numbered(60, None))]);
        let c2 = commit_files(&repo, "c2", &[c1], "HEAD", &[("f.txt", &numbered(60, Some((30, "CHANGED"))))]);
        let r = Repo::open(dir.path()).unwrap();

        let normal = &r.commit_detail(&c2.to_string()).unwrap().files[0];
        let normal_lines = normal.lines.iter().filter(|l| l.origin != 'H').count();
        assert!(normal_lines < 20, "{normal_lines}");

        let (full, truncated) = r.file_diff_full(&c2.to_string(), "f.txt", None).unwrap();
        assert!(!truncated);
        let content: Vec<_> = full.lines.iter().filter(|l| l.origin != 'H').collect();
        assert_eq!(content.iter().filter(|l| l.origin == ' ').count(), 59); // all unchanged lines
        assert_eq!(content.iter().filter(|l| l.origin == '-').count(), 1);
        assert_eq!(content.iter().filter(|l| l.origin == '+').count(), 1);
        assert_eq!((full.additions, full.deletions), (1, 1));
        // New-file order is preserved: line 1 first, the 60th last, change in the middle.
        assert_eq!(content.first().unwrap().text, "line 1");
        assert_eq!(content.last().unwrap().text, "line 60");
        assert!(full.lines.iter().filter(|l| l.origin == 'H').count() == 1, "a single hunk");
    }

    #[test]
    fn full_diff_of_added_and_deleted_files() {
        let (dir, repo) = init();
        let c1 = commit_files(&repo, "c1", &[], "HEAD", &[("old.txt", "a\nb\n")]);
        let c2 = commit_files(&repo, "c2", &[c1], "HEAD", &[("new.txt", "x\ny\nz\n")]);
        let r = Repo::open(dir.path()).unwrap();
        let (added, _) = r.file_diff_full(&c2.to_string(), "new.txt", None).unwrap();
        assert_eq!(added.status, 'A');
        assert!(added.lines.iter().filter(|l| l.origin != 'H').all(|l| l.origin == '+'));
        let (gone, _) = r.file_diff_full(&c2.to_string(), "old.txt", None).unwrap();
        assert_eq!(gone.status, 'D');
        assert_eq!(gone.deletions, 2);
    }

    #[test]
    fn full_diff_of_a_renamed_file_keeps_unchanged_lines_as_context() {
        let (dir, repo) = init();
        let body = numbered(30, None);
        let c1 = commit_files(&repo, "c1", &[], "HEAD", &[("before.txt", &body)]);
        let c2 = commit_files(&repo, "c2", &[c1], "HEAD", &[("after.txt", &numbered(30, Some((10, "EDITED"))))]);
        let r = Repo::open(dir.path()).unwrap();
        let detail = r.commit_detail(&c2.to_string()).unwrap();
        let f = &detail.files[0];
        assert_eq!((f.status, f.path.as_str(), f.old_path.as_deref()), ('R', "after.txt", Some("before.txt")));

        let (full, _) = r.file_diff_full(&c2.to_string(), &f.path, f.old_path.as_deref()).unwrap();
        assert_eq!(full.status, 'R');
        assert_eq!(full.lines.iter().filter(|l| l.origin == ' ').count(), 29);
    }

    #[test]
    fn full_diff_of_a_root_commit_and_of_unknown_files() {
        let (dir, repo) = init();
        let c1 = commit_files(&repo, "c1", &[], "HEAD", &[("a.txt", "1\n2\n")]);
        let r = Repo::open(dir.path()).unwrap();
        let (full, _) = r.file_diff_full(&c1.to_string(), "a.txt", None).unwrap();
        assert_eq!(full.additions, 2);
        assert!(r.file_diff_full(&c1.to_string(), "nope.txt", None).is_err());
        assert!(r.file_diff_full("not-an-id", "a.txt", None).is_err());
    }

    #[test]
    fn full_diff_does_not_treat_the_path_as_a_glob() {
        let (dir, repo) = init();
        let c1 = commit_files(&repo, "c1", &[], "HEAD", &[("a[1].txt", "x\n"), ("a1.txt", "y\n")]);
        let r = Repo::open(dir.path()).unwrap();
        let (f, _) = r.file_diff_full(&c1.to_string(), "a[1].txt", None).unwrap();
        assert_eq!(f.path, "a[1].txt");
        assert_eq!(f.additions, 1);
    }
}
