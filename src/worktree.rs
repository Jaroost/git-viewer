//! Operations that change the repository: staging, commits, branches, remotes.

use crate::repo::{files_from_diff, FileDiff, Repo};
use anyhow::{bail, Context, Result};
use git2::{
    build::CheckoutBuilder, ErrorCode, Repository, StashFlags, Branch, BranchType, Cred, CredentialType, DiffOptions, FetchOptions,
    ObjectType, Oid, PushOptions, RemoteCallbacks, Status, StatusOptions,
};
use std::path::Path;

/// An entry of the stash list; `index` 0 is the most recent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StashEntry {
    pub index: usize,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StatusEntry {
    pub path: String,
    /// Change recorded in the index: 'A', 'M', 'D', 'R' or 'T'.
    pub staged: Option<char>,
    /// Change in the working tree: '?' (untracked), 'M', 'D', 'R', 'T' or 'U' (conflict).
    pub unstaged: Option<char>,
}

impl Repo {
    // ---------------------------------------------------------------- status

    pub fn current_branch(&self) -> Option<String> {
        let head = self.inner.head().ok()?;
        head.is_branch().then(|| head.shorthand().ok().map(str::to_string)).flatten()
    }

    pub fn status(&self) -> Result<Vec<StatusEntry>> {
        let mut opts = StatusOptions::new();
        opts.include_untracked(true)
            .recurse_untracked_dirs(true)
            .renames_head_to_index(true)
            .include_ignored(false);
        let statuses = self.inner.statuses(Some(&mut opts))?;
        let mut out = Vec::new();
        for e in statuses.iter() {
            let s = e.status();
            let Ok(path) = e.path() else { continue };
            let staged = [
                (Status::INDEX_NEW, 'A'),
                (Status::INDEX_MODIFIED, 'M'),
                (Status::INDEX_DELETED, 'D'),
                (Status::INDEX_RENAMED, 'R'),
                (Status::INDEX_TYPECHANGE, 'T'),
            ]
            .into_iter()
            .find(|(f, _)| s.contains(*f))
            .map(|(_, c)| c);
            let unstaged = if s.contains(Status::CONFLICTED) {
                Some('U')
            } else {
                [
                    (Status::WT_NEW, '?'),
                    (Status::WT_MODIFIED, 'M'),
                    (Status::WT_DELETED, 'D'),
                    (Status::WT_RENAMED, 'R'),
                    (Status::WT_TYPECHANGE, 'T'),
                ]
                .into_iter()
                .find(|(f, _)| s.contains(*f))
                .map(|(_, c)| c)
            };
            if staged.is_some() || unstaged.is_some() {
                out.push(StatusEntry { path: path.to_string(), staged, unstaged });
            }
        }
        out.sort_by(|a, b| a.path.cmp(&b.path));
        Ok(out)
    }

    pub fn diff_staged(&self) -> Result<Vec<FileDiff>> {
        let head_tree = self.inner.head().ok().and_then(|h| h.peel_to_tree().ok());
        let mut diff = self.inner.diff_tree_to_index(head_tree.as_ref(), None, None)?;
        diff.find_similar(None)?;
        Ok(files_from_diff(&diff)?.0)
    }

    pub fn diff_unstaged(&self) -> Result<Vec<FileDiff>> {
        let mut opts = DiffOptions::new();
        opts.include_untracked(true).recurse_untracked_dirs(true).show_untracked_content(true);
        let diff = self.inner.diff_index_to_workdir(None, Some(&mut opts))?;
        Ok(files_from_diff(&diff)?.0)
    }

    // --------------------------------------------------------------- staging

    pub fn stage(&self, path: &str) -> Result<()> {
        let mut index = self.inner.index()?;
        let full = self.inner.workdir().context("dépôt sans working tree")?.join(path);
        if full.symlink_metadata().is_ok() {
            index.add_path(Path::new(path))?;
        } else {
            index.remove_path(Path::new(path))?; // deleted file
        }
        index.write()?;
        Ok(())
    }

    pub fn unstage(&self, path: &str) -> Result<()> {
        match self.inner.head().ok().and_then(|h| h.peel(ObjectType::Commit).ok()) {
            Some(commit) => self.inner.reset_default(Some(&commit), [path])?,
            None => {
                // No commit yet: unstaging means removing from the index.
                let mut index = self.inner.index()?;
                index.remove_path(Path::new(path))?;
                index.write()?;
            }
        }
        Ok(())
    }

    pub fn stage_all(&self) -> Result<()> {
        let mut index = self.inner.index()?;
        index.add_all(["*"], git2::IndexAddOption::DEFAULT, None)?;
        index.update_all(["*"], None)?;
        index.write()?;
        Ok(())
    }

    pub fn unstage_all(&self) -> Result<()> {
        for e in self.status()? {
            if e.staged.is_some() {
                self.unstage(&e.path)?;
            }
        }
        Ok(())
    }

    // --------------------------------------------------------------- commits

    pub fn commit(&self, message: &str) -> Result<Oid> {
        let message = message.trim();
        if message.is_empty() {
            bail!("le message de commit est vide");
        }
        let sig = self.inner.signature().context(
            "identité git manquante : configure user.name et user.email (git config --global ...)",
        )?;
        let mut index = self.inner.index()?;
        if index.has_conflicts() {
            bail!("des conflits non résolus empêchent de commiter");
        }
        let tree_id = index.write_tree()?;
        let tree = self.inner.find_tree(tree_id)?;
        let parent = self.inner.head().ok().and_then(|h| h.peel_to_commit().ok());
        match &parent {
            Some(p) if p.tree_id() == tree_id => bail!("rien à commiter (aucun fichier indexé)"),
            None if index.is_empty() => bail!("rien à commiter (aucun fichier indexé)"),
            _ => {}
        }
        let parents: Vec<_> = parent.iter().collect();
        Ok(self.inner.commit(Some("HEAD"), &sig, &sig, message, &tree, &parents)?)
    }

    // -------------------------------------------------------------- branches

    /// Creates a branch at `from` (a commit id) or at HEAD.
    pub fn create_branch(&self, name: &str, from: Option<&str>) -> Result<()> {
        if !Branch::name_is_valid(name)? {
            bail!("nom de branche invalide : « {name} »");
        }
        let commit = match from {
            Some(id) => self.inner.find_commit(Oid::from_str(id)?)?,
            None => self.inner.head()?.peel_to_commit().context("aucun commit sur HEAD")?,
        };
        self.inner.branch(name, &commit, false).with_context(|| format!("création de « {name} » impossible"))?;
        Ok(())
    }

    /// Safe checkout: refuses to overwrite local changes.
    pub fn checkout_branch(&self, name: &str) -> Result<()> {
        let branch = self.inner.find_branch(name, BranchType::Local)?;
        let refname = branch.get().name().context("nom de référence invalide")?.to_string();
        let target = branch.get().peel(ObjectType::Commit)?;
        self.inner
            .checkout_tree(&target, Some(CheckoutBuilder::new().safe()))
            .context("checkout refusé : des modifications locales seraient écrasées")?;
        self.inner.set_head(&refname)?;
        Ok(())
    }

    pub fn checkout_commit(&self, id: &str) -> Result<()> {
        let commit = self.inner.find_commit(Oid::from_str(id)?)?;
        self.inner
            .checkout_tree(commit.as_object(), Some(CheckoutBuilder::new().safe()))
            .context("checkout refusé : des modifications locales seraient écrasées")?;
        self.inner.set_head_detached(commit.id())?;
        Ok(())
    }

    /// True if every commit of the local branch is reachable from HEAD (safe to delete).
    pub fn is_merged(&self, name: &str) -> Result<bool> {
        let tip = self.inner.find_branch(name, BranchType::Local)?.get().peel_to_commit()?.id();
        let head = self.inner.head()?.peel_to_commit()?.id();
        Ok(tip == head || self.inner.graph_descendant_of(head, tip)?)
    }

    pub fn delete_branch(&self, name: &str) -> Result<()> {
        let mut branch = self.inner.find_branch(name, BranchType::Local)?;
        if branch.is_head() {
            bail!("impossible de supprimer la branche courante");
        }
        branch.delete()?;
        Ok(())
    }

    /// Checks out `remote/name`: reuses the local branch of that name if there is one, otherwise
    /// creates it at the remote's commit, tracking it, then switches to it.
    pub fn checkout_remote_branch(&self, remote: &str, name: &str) -> Result<()> {
        if self.inner.find_branch(name, BranchType::Local).is_err() {
            let full = format!("{remote}/{name}");
            let rb = self
                .inner
                .find_branch(&full, BranchType::Remote)
                .with_context(|| format!("branche distante « {full} » introuvable"))?;
            let commit = rb.get().peel_to_commit()?;
            let mut local = self.inner.branch(name, &commit, false)?;
            local.set_upstream(Some(&full))?;
        }
        self.checkout_branch(name)
    }

    pub fn create_tag(&self, name: &str, id: &str) -> Result<()> {
        let obj = self.inner.find_object(Oid::from_str(id)?, Some(ObjectType::Commit))?;
        self.inner.tag_lightweight(name, &obj, false)?;
        Ok(())
    }

    // ----------------------------------------------------------------- stash
    // libgit2 wants `&mut Repository` for stashes; a second handle on the same repo is cheap.

    fn stash_handle(&self) -> Result<Repository> {
        Ok(Repository::open(self.inner.path())?)
    }

    /// Stashes tracked changes (and untracked files if asked) and cleans the working tree.
    pub fn stash_save(&self, message: &str, include_untracked: bool) -> Result<()> {
        let mut repo = self.stash_handle()?;
        let sig = repo.signature().context(
            "identité git manquante : configure user.name et user.email (git config --global ...)",
        )?;
        let message = message.trim();
        let flags = include_untracked.then_some(StashFlags::INCLUDE_UNTRACKED);
        match repo.stash_save2(&sig, (!message.is_empty()).then_some(message), flags) {
            Ok(_) => Ok(()),
            Err(e) if e.code() == ErrorCode::NotFound => {
                bail!("rien à mettre de côté : aucune modification")
            }
            Err(e) => Err(e.into()),
        }
    }

    pub fn stash_list(&self) -> Result<Vec<StashEntry>> {
        let mut repo = self.stash_handle()?;
        let mut out = Vec::new();
        repo.stash_foreach(|index, message, _| {
            out.push(StashEntry { index, message: message.to_string() });
            true
        })?;
        Ok(out)
    }

    /// Applies a stash and keeps it in the list.
    pub fn stash_apply(&self, index: usize) -> Result<()> {
        self.stash_handle()?
            .stash_apply(index, None)
            .context("application du stash impossible (conflits avec tes modifications ?)")
    }

    /// Applies a stash then removes it. On failure (conflicts) the stash is kept.
    pub fn stash_pop(&self, index: usize) -> Result<()> {
        self.stash_handle()?
            .stash_pop(index, None)
            .context("pop du stash impossible (conflits avec tes modifications ?) : il est conservé")
    }

    pub fn stash_drop(&self, index: usize) -> Result<()> {
        Ok(self.stash_handle()?.stash_drop(index)?)
    }

    // --------------------------------------------------------------- remotes

    pub fn remotes(&self) -> Result<Vec<String>> {
        Ok(self.inner.remotes()?.iter().flatten().flatten().map(str::to_string).collect())
    }

    pub fn fetch(&self, remote: &str) -> Result<()> {
        let mut r = self.inner.find_remote(remote)?;
        let mut opts = FetchOptions::new();
        opts.remote_callbacks(self.callbacks());
        opts.prune(git2::FetchPrune::On);
        r.fetch(&[] as &[&str], Some(&mut opts), None)
            .with_context(|| format!("fetch depuis « {remote} » impossible"))?;
        Ok(())
    }

    /// Pushes a local branch to `remote` and sets its upstream if it had none.
    pub fn push(&self, remote: &str, branch: &str) -> Result<()> {
        let mut r = self.inner.find_remote(remote)?;
        let rejected = std::cell::RefCell::new(None::<String>);
        {
            let mut cb = self.callbacks();
            cb.push_update_reference(|refname, status| {
                if let Some(msg) = status {
                    *rejected.borrow_mut() = Some(format!("{refname} : {msg}"));
                }
                Ok(())
            });
            let mut opts = PushOptions::new();
            opts.remote_callbacks(cb);
            let spec = format!("refs/heads/{branch}:refs/heads/{branch}");
            r.push(&[spec], Some(&mut opts))
                .with_context(|| format!("push vers « {remote} » impossible"))?;
        }
        if let Some(msg) = rejected.into_inner() {
            bail!("push rejeté par le serveur — {msg}");
        }
        let mut b = self.inner.find_branch(branch, BranchType::Local)?;
        if b.upstream().is_err() {
            b.set_upstream(Some(&format!("{remote}/{branch}")))?;
        }
        Ok(())
    }

    /// Fetch + fast-forward of the current branch onto its upstream.
    pub fn pull(&self) -> Result<String> {
        let name = self.current_branch().context("HEAD n'est sur aucune branche")?;
        let branch = self.inner.find_branch(&name, BranchType::Local)?;
        let upstream = branch
            .upstream()
            .with_context(|| format!("« {name} » n'a pas de branche amont : fais d'abord un push"))?;
        let remote_name = self
            .inner
            .branch_upstream_remote(branch.get().name().context("référence invalide")?)?
            .as_str()
            .context("nom de remote invalide")?
            .to_string();
        self.fetch(&remote_name)?;

        // Re-read the upstream after the fetch.
        let upstream = self
            .inner
            .find_branch(upstream.name()?.context("nom invalide")?, BranchType::Remote)?;
        let target = self.inner.reference_to_annotated_commit(upstream.get())?;
        let (analysis, _) = self.inner.merge_analysis(&[&target])?;
        if analysis.is_up_to_date() {
            return Ok("déjà à jour".into());
        }
        if analysis.is_fast_forward() {
            let mut local = self.inner.find_branch(&name, BranchType::Local)?;
            let obj = self.inner.find_object(target.id(), Some(ObjectType::Commit))?;
            self.inner
                .checkout_tree(&obj, Some(CheckoutBuilder::new().safe()))
                .context("pull refusé : des modifications locales seraient écrasées")?;
            local.get_mut().set_target(target.id(), "pull: fast-forward")?;
            return Ok("fast-forward effectué".into());
        }
        bail!("les branches ont divergé : une fusion manuelle est nécessaire")
    }

    /// Authentication: ssh-agent, then default key files, then the git credential helper.
    fn callbacks(&self) -> RemoteCallbacks<'_> {
        let mut attempts = 0u32;
        let mut cb = RemoteCallbacks::new();
        cb.credentials(move |url, username, allowed| {
            attempts += 1;
            if attempts > 4 {
                return Err(git2::Error::from_str("authentification échouée"));
            }
            if allowed.contains(CredentialType::SSH_KEY) {
                let user = username.unwrap_or("git");
                if attempts == 1 {
                    return Cred::ssh_key_from_agent(user);
                }
                if let Some(home) = dirs::home_dir() {
                    for key in ["id_ed25519", "id_rsa"] {
                        let path = home.join(".ssh").join(key);
                        if path.exists() && attempts == 2 + (key == "id_rsa") as u32 {
                            return Cred::ssh_key(user, None, &path, None);
                        }
                    }
                }
            }
            if allowed.contains(CredentialType::USER_PASS_PLAINTEXT) {
                if let Ok(cfg) = self.inner.config() {
                    return Cred::credential_helper(&cfg, url, username);
                }
            }
            if allowed.contains(CredentialType::DEFAULT) {
                return Cred::default();
            }
            Err(git2::Error::from_str("aucune méthode d'authentification disponible"))
        });
        cb
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use git2::Repository;
    use tempfile::TempDir;

    /// A repo with a configured identity.
    fn init() -> (TempDir, Repo) {
        let dir = TempDir::new().unwrap();
        let raw = Repository::init(dir.path()).unwrap();
        let mut cfg = raw.config().unwrap();
        cfg.set_str("user.name", "Tester").unwrap();
        cfg.set_str("user.email", "t@example.com").unwrap();
        let repo = Repo::open(dir.path()).unwrap();
        (dir, repo)
    }

    fn write(dir: &TempDir, name: &str, content: &str) {
        std::fs::write(dir.path().join(name), content).unwrap();
    }

    fn read(dir: &TempDir, name: &str) -> String {
        std::fs::read_to_string(dir.path().join(name)).unwrap()
    }

    fn commit_file(dir: &TempDir, repo: &Repo, name: &str, content: &str, msg: &str) -> Oid {
        write(dir, name, content);
        repo.stage(name).unwrap();
        repo.commit(msg).unwrap()
    }

    fn entry(repo: &Repo, path: &str) -> Option<StatusEntry> {
        repo.status().unwrap().into_iter().find(|e| e.path == path)
    }

    // ------------------------------------------------------ status & staging

    #[test]
    fn untracked_file_is_unstaged_then_staged_then_clean() {
        let (dir, repo) = init();
        write(&dir, "a.txt", "hello\n");
        let e = entry(&repo, "a.txt").unwrap();
        assert_eq!((e.staged, e.unstaged), (None, Some('?')));

        repo.stage("a.txt").unwrap();
        let e = entry(&repo, "a.txt").unwrap();
        assert_eq!((e.staged, e.unstaged), (Some('A'), None));

        repo.commit("add a").unwrap();
        assert!(repo.status().unwrap().is_empty());
    }

    #[test]
    fn modified_and_deleted_files_are_reported_and_stageable() {
        let (dir, repo) = init();
        commit_file(&dir, &repo, "a.txt", "1\n", "c1");
        commit_file(&dir, &repo, "b.txt", "1\n", "c2");
        write(&dir, "a.txt", "2\n");
        std::fs::remove_file(dir.path().join("b.txt")).unwrap();
        assert_eq!(entry(&repo, "a.txt").unwrap().unstaged, Some('M'));
        assert_eq!(entry(&repo, "b.txt").unwrap().unstaged, Some('D'));

        repo.stage("a.txt").unwrap();
        repo.stage("b.txt").unwrap();
        assert_eq!(entry(&repo, "a.txt").unwrap().staged, Some('M'));
        assert_eq!(entry(&repo, "b.txt").unwrap().staged, Some('D'));
    }

    #[test]
    fn unstage_works_with_and_without_a_head_commit() {
        let (dir, repo) = init();
        write(&dir, "a.txt", "1\n");
        repo.stage("a.txt").unwrap();
        repo.unstage("a.txt").unwrap(); // no commit yet
        assert_eq!(entry(&repo, "a.txt").unwrap().staged, None);

        repo.stage("a.txt").unwrap();
        repo.commit("c1").unwrap();
        write(&dir, "a.txt", "2\n");
        repo.stage("a.txt").unwrap();
        repo.unstage("a.txt").unwrap();
        let e = entry(&repo, "a.txt").unwrap();
        assert_eq!((e.staged, e.unstaged), (None, Some('M')));
        assert_eq!(read(&dir, "a.txt"), "2\n"); // content is kept
    }

    #[test]
    fn stage_all_and_unstage_all() {
        let (dir, repo) = init();
        write(&dir, "a.txt", "1\n");
        write(&dir, "b.txt", "1\n");
        repo.stage_all().unwrap();
        assert!(repo.status().unwrap().iter().all(|e| e.staged == Some('A')));
        repo.unstage_all().unwrap();
        assert!(repo.status().unwrap().iter().all(|e| e.staged.is_none()));
    }

    #[test]
    fn working_tree_diffs_are_split_between_staged_and_unstaged() {
        let (dir, repo) = init();
        commit_file(&dir, &repo, "a.txt", "one\n", "c1");
        write(&dir, "a.txt", "two\n");
        repo.stage("a.txt").unwrap();
        write(&dir, "a.txt", "three\n");
        write(&dir, "new.txt", "fresh\n");

        let staged = repo.diff_staged().unwrap();
        assert_eq!(staged.len(), 1);
        assert!(staged[0].lines.iter().any(|l| l.origin == '+' && l.text == "two"));

        let unstaged = repo.diff_unstaged().unwrap();
        let a = unstaged.iter().find(|f| f.path == "a.txt").unwrap();
        assert!(a.lines.iter().any(|l| l.origin == '+' && l.text == "three"));
        let n = unstaged.iter().find(|f| f.path == "new.txt").unwrap();
        assert!(n.lines.iter().any(|l| l.origin == '+' && l.text == "fresh"));
    }

    // --------------------------------------------------------------- commits

    #[test]
    fn commit_records_message_parent_and_author() {
        let (dir, repo) = init();
        let c1 = commit_file(&dir, &repo, "a.txt", "1\n", "first\n\nbody");
        let c2 = commit_file(&dir, &repo, "a.txt", "2\n", "second");
        let d = repo.commit_detail(&c2.to_string()).unwrap();
        assert_eq!(d.parents, [c1.to_string()]);
        assert_eq!(d.author, "Tester");
        assert_eq!(repo.commit_detail(&c1.to_string()).unwrap().message, "first\n\nbody");
    }

    #[test]
    fn commit_rejects_empty_message_and_empty_index() {
        let (dir, repo) = init();
        write(&dir, "a.txt", "1\n");
        repo.stage("a.txt").unwrap();
        assert!(repo.commit("   ").is_err());
        assert!(init().1.commit("nothing staged").is_err()); // initial commit, empty index
        repo.commit("ok").unwrap();
        assert!(repo.commit("again").is_err()); // nothing new staged
    }

    // -------------------------------------------------------------- branches

    #[test]
    fn create_checkout_and_delete_branch() {
        let (dir, repo) = init();
        commit_file(&dir, &repo, "a.txt", "main\n", "c1");
        let main = repo.current_branch().unwrap();

        repo.create_branch("feature", None).unwrap();
        repo.checkout_branch("feature").unwrap();
        assert_eq!(repo.current_branch().as_deref(), Some("feature"));
        commit_file(&dir, &repo, "a.txt", "feature\n", "c2");

        repo.checkout_branch(&main).unwrap();
        assert_eq!(read(&dir, "a.txt"), "main\n"); // working tree follows the branch
        repo.checkout_branch("feature").unwrap();
        assert_eq!(read(&dir, "a.txt"), "feature\n");

        assert!(repo.delete_branch("feature").is_err()); // current branch
        repo.checkout_branch(&main).unwrap();
        repo.delete_branch("feature").unwrap();
        assert!(repo.branches().unwrap().iter().all(|b| b.name != "feature"));
    }

    #[test]
    fn create_branch_validates_name_and_rejects_duplicates() {
        let (dir, repo) = init();
        commit_file(&dir, &repo, "a.txt", "1\n", "c1");
        assert!(repo.create_branch("bad name", None).is_err());
        assert!(repo.create_branch("a..b", None).is_err());
        repo.create_branch("ok", None).unwrap();
        assert!(repo.create_branch("ok", None).is_err());
    }

    #[test]
    fn create_branch_from_a_given_commit() {
        let (dir, repo) = init();
        let c1 = commit_file(&dir, &repo, "a.txt", "1\n", "c1");
        commit_file(&dir, &repo, "a.txt", "2\n", "c2");
        repo.create_branch("old", Some(&c1.to_string())).unwrap();
        repo.checkout_branch("old").unwrap();
        assert_eq!(read(&dir, "a.txt"), "1\n");
    }

    #[test]
    fn checkout_refuses_to_overwrite_local_changes() {
        let (dir, repo) = init();
        commit_file(&dir, &repo, "a.txt", "main\n", "c1");
        let main = repo.current_branch().unwrap();
        repo.create_branch("feature", None).unwrap();
        repo.checkout_branch("feature").unwrap();
        commit_file(&dir, &repo, "a.txt", "feature\n", "c2");
        repo.checkout_branch(&main).unwrap();

        write(&dir, "a.txt", "dirty local edit\n");
        assert!(repo.checkout_branch("feature").is_err());
        assert_eq!(repo.current_branch().as_deref(), Some(main.as_str()));
        assert_eq!(read(&dir, "a.txt"), "dirty local edit\n"); // nothing lost
    }

    #[test]
    fn detached_checkout_and_tag() {
        let (dir, repo) = init();
        let c1 = commit_file(&dir, &repo, "a.txt", "1\n", "c1");
        commit_file(&dir, &repo, "a.txt", "2\n", "c2");
        repo.create_tag("v1", &c1.to_string()).unwrap();
        assert!(repo.create_tag("v1", &c1.to_string()).is_err());
        repo.checkout_commit(&c1.to_string()).unwrap();
        assert_eq!(repo.current_branch(), None);
        assert_eq!(read(&dir, "a.txt"), "1\n");
    }

    // --------------------------------------------------------------- remotes

    /// A bare "server" plus a working repo that has it as `origin`.
    fn with_origin() -> (TempDir, TempDir, Repo) {
        let server = TempDir::new().unwrap();
        Repository::init_bare(server.path()).unwrap();
        let (dir, repo) = init();
        repo.inner.remote("origin", server.path().to_str().unwrap()).unwrap();
        (server, dir, repo)
    }

    fn clone_of(server: &TempDir) -> (TempDir, Repo) {
        let dir = TempDir::new().unwrap();
        let raw = Repository::clone(server.path().to_str().unwrap(), dir.path()).unwrap();
        let mut cfg = raw.config().unwrap();
        cfg.set_str("user.name", "Other").unwrap();
        cfg.set_str("user.email", "o@example.com").unwrap();
        let repo = Repo::open(dir.path()).unwrap();
        (dir, repo)
    }

    #[test]
    fn push_publishes_the_branch_and_sets_upstream() {
        let (server, dir, repo) = with_origin();
        let c1 = commit_file(&dir, &repo, "a.txt", "1\n", "c1");
        let main = repo.current_branch().unwrap();
        repo.push("origin", &main).unwrap();

        let bare = Repository::open_bare(server.path()).unwrap();
        assert_eq!(bare.refname_to_id(&format!("refs/heads/{main}")).unwrap(), c1);
        let local = repo.inner.find_branch(&main, BranchType::Local).unwrap();
        assert!(local.upstream().is_ok());
    }

    #[test]
    fn fetch_and_pull_fast_forward_from_another_clone() {
        let (server, dir, repo) = with_origin();
        commit_file(&dir, &repo, "a.txt", "1\n", "c1");
        let main = repo.current_branch().unwrap();
        repo.push("origin", &main).unwrap();
        assert_eq!(repo.pull().unwrap(), "déjà à jour");

        let (odir, other) = clone_of(&server);
        commit_file(&odir, &other, "a.txt", "2\n", "from other");
        other.push("origin", &main).unwrap();

        repo.fetch("origin").unwrap();
        assert_eq!(read(&dir, "a.txt"), "1\n"); // fetch alone does not touch the tree
        assert_eq!(repo.pull().unwrap(), "fast-forward effectué");
        assert_eq!(read(&dir, "a.txt"), "2\n");
        assert_eq!(repo.commits(10).unwrap().len(), 2);
    }

    #[test]
    fn pull_reports_divergence_instead_of_merging() {
        let (server, dir, repo) = with_origin();
        commit_file(&dir, &repo, "a.txt", "1\n", "c1");
        let main = repo.current_branch().unwrap();
        repo.push("origin", &main).unwrap();

        let (odir, other) = clone_of(&server);
        commit_file(&odir, &other, "b.txt", "other\n", "remote work");
        other.push("origin", &main).unwrap();
        commit_file(&dir, &repo, "c.txt", "local\n", "local work");

        let err = repo.pull().unwrap_err().to_string();
        assert!(err.contains("divergé"), "{err}");
        assert!(dir.path().join("c.txt").exists()); // local work untouched
    }

    #[test]
    fn non_fast_forward_push_is_rejected() {
        let (server, dir, repo) = with_origin();
        commit_file(&dir, &repo, "a.txt", "1\n", "c1");
        let main = repo.current_branch().unwrap();
        repo.push("origin", &main).unwrap();

        let (odir, other) = clone_of(&server);
        commit_file(&odir, &other, "b.txt", "other\n", "remote work");
        other.push("origin", &main).unwrap();

        commit_file(&dir, &repo, "c.txt", "local\n", "local work");
        assert!(repo.push("origin", &main).is_err());
    }

    #[test]
    fn pull_without_upstream_explains_what_to_do() {
        let (dir, repo) = init();
        commit_file(&dir, &repo, "a.txt", "1\n", "c1");
        let err = repo.pull().unwrap_err().to_string();
        assert!(err.contains("amont"), "{err}");
    }

    #[test]
    fn remotes_lists_configured_names() {
        let (_server, _dir, repo) = with_origin();
        assert_eq!(repo.remotes().unwrap(), ["origin"]);
    }

    #[test]
    fn is_merged_tells_if_a_branch_would_lose_commits() {
        let (dir, repo) = init();
        commit_file(&dir, &repo, "a.txt", "1\n", "c1");
        let main = repo.current_branch().unwrap();
        repo.create_branch("same", None).unwrap();
        assert!(repo.is_merged("same").unwrap()); // same commit as HEAD

        repo.create_branch("feature", None).unwrap();
        repo.checkout_branch("feature").unwrap();
        commit_file(&dir, &repo, "a.txt", "2\n", "only on feature");
        repo.checkout_branch(&main).unwrap();
        assert!(!repo.is_merged("feature").unwrap()); // has a commit HEAD lacks

        // After merging (fast-forward main onto feature) it is merged.
        repo.checkout_branch("feature").unwrap();
        repo.create_branch("tmp", None).unwrap();
        assert!(repo.is_merged(&main).unwrap()); // main is an ancestor of feature's HEAD
        assert!(repo.is_merged("tmp").unwrap());
    }

    #[test]
    fn is_merged_errors_on_unknown_branch() {
        let (dir, repo) = init();
        commit_file(&dir, &repo, "a.txt", "1\n", "c1");
        assert!(repo.is_merged("nope").is_err());
    }

    // ------------------------------------------------------- remote branches

    #[test]
    fn checkout_remote_branch_creates_a_tracking_local_branch() {
        let (server, dir, repo) = with_origin();
        commit_file(&dir, &repo, "a.txt", "1\n", "c1");
        let main = repo.current_branch().unwrap();
        repo.push("origin", &main).unwrap();

        // Another clone publishes a new branch.
        let (odir, other) = clone_of(&server);
        other.create_branch("feature", None).unwrap();
        other.checkout_branch("feature").unwrap();
        commit_file(&odir, &other, "f.txt", "feature work\n", "on feature");
        other.push("origin", "feature").unwrap();

        repo.fetch("origin").unwrap();
        assert!(repo.branches().unwrap().iter().all(|b| b.name != "feature"));
        repo.checkout_remote_branch("origin", "feature").unwrap();
        assert_eq!(repo.current_branch().as_deref(), Some("feature"));
        assert_eq!(read(&dir, "f.txt"), "feature work\n");
        let b = repo.branches().unwrap().into_iter().find(|b| b.name == "feature").unwrap();
        assert!(b.has_upstream);

        // Second time: the local branch already exists and is simply checked out.
        repo.checkout_branch(&main).unwrap();
        repo.checkout_remote_branch("origin", "feature").unwrap();
        assert_eq!(repo.current_branch().as_deref(), Some("feature"));
        assert!(repo.checkout_remote_branch("origin", "nope").is_err());
    }

    #[test]
    fn a_new_branch_that_is_not_checked_out_can_be_published() {
        let (server, dir, repo) = with_origin();
        let c1 = commit_file(&dir, &repo, "a.txt", "1\n", "c1");
        repo.create_branch("brand-new", None).unwrap(); // not checked out
        assert!(!repo.branches().unwrap().iter().find(|b| b.name == "brand-new").unwrap().has_upstream);

        repo.push("origin", "brand-new").unwrap();
        let bare = Repository::open_bare(server.path()).unwrap();
        assert_eq!(bare.refname_to_id("refs/heads/brand-new").unwrap(), c1);
        assert!(repo.branches().unwrap().iter().find(|b| b.name == "brand-new").unwrap().has_upstream);
        // The branch we are on was not published by this.
        let main = repo.current_branch().unwrap();
        assert!(bare.refname_to_id(&format!("refs/heads/{main}")).is_err());
    }

    #[test]
    fn publishing_an_unknown_branch_fails_cleanly() {
        let (_server, dir, repo) = with_origin();
        commit_file(&dir, &repo, "a.txt", "1\n", "c1");
        assert!(repo.push("origin", "does-not-exist").is_err());
        assert!(repo.push("no-such-remote", "master").is_err());
    }

    // ----------------------------------------------------------------- stash

    #[test]
    fn stash_save_cleans_the_tree_and_lists_the_entry() {
        let (dir, repo) = init();
        commit_file(&dir, &repo, "a.txt", "1\n", "c1");
        write(&dir, "a.txt", "changed\n");
        repo.stash_save("my work", false).unwrap();
        assert_eq!(read(&dir, "a.txt"), "1\n");
        assert!(repo.status().unwrap().is_empty());
        let list = repo.stash_list().unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].index, 0);
        assert!(list[0].message.contains("my work"), "{}", list[0].message);
    }

    #[test]
    fn stash_without_message_gets_a_default_one_and_nothing_to_stash_is_an_error() {
        let (dir, repo) = init();
        commit_file(&dir, &repo, "a.txt", "1\n", "c1");
        let err = repo.stash_save("", false).unwrap_err().to_string();
        assert!(err.contains("rien à mettre de côté"), "{err}");
        write(&dir, "a.txt", "changed\n");
        repo.stash_save("   ", false).unwrap();
        assert!(!repo.stash_list().unwrap()[0].message.is_empty());
    }

    #[test]
    fn untracked_files_are_stashed_only_when_asked() {
        let (dir, repo) = init();
        commit_file(&dir, &repo, "a.txt", "1\n", "c1");
        write(&dir, "new.txt", "untracked\n");
        // Only an untracked file: without the flag there is nothing to stash.
        assert!(repo.stash_save("x", false).is_err());
        assert!(dir.path().join("new.txt").exists());
        repo.stash_save("x", true).unwrap();
        assert!(!dir.path().join("new.txt").exists());
        repo.stash_pop(0).unwrap();
        assert_eq!(read(&dir, "new.txt"), "untracked\n");
    }

    #[test]
    fn apply_keeps_the_stash_and_pop_removes_it() {
        let (dir, repo) = init();
        commit_file(&dir, &repo, "a.txt", "1\n", "c1");
        write(&dir, "a.txt", "changed\n");
        repo.stash_save("w", false).unwrap();

        repo.stash_apply(0).unwrap();
        assert_eq!(read(&dir, "a.txt"), "changed\n");
        assert_eq!(repo.stash_list().unwrap().len(), 1); // still there

        write(&dir, "a.txt", "1\n"); // back to clean content
        repo.stage("a.txt").unwrap();
        repo.unstage("a.txt").unwrap();
        repo.stash_pop(0).unwrap();
        assert_eq!(read(&dir, "a.txt"), "changed\n");
        assert!(repo.stash_list().unwrap().is_empty());
    }

    #[test]
    fn newest_stash_has_index_zero_and_drop_removes_the_right_one() {
        let (dir, repo) = init();
        commit_file(&dir, &repo, "a.txt", "1\n", "c1");
        write(&dir, "a.txt", "first\n");
        repo.stash_save("first", false).unwrap();
        write(&dir, "a.txt", "second\n");
        repo.stash_save("second", false).unwrap();

        let list = repo.stash_list().unwrap();
        assert!(list[0].message.contains("second") && list[1].message.contains("first"));
        repo.stash_drop(0).unwrap();
        let list = repo.stash_list().unwrap();
        assert_eq!(list.len(), 1);
        assert!(list[0].message.contains("first"));
        assert!(repo.stash_drop(5).is_err());
    }

    #[test]
    fn pop_with_conflicting_local_changes_fails_and_keeps_the_stash() {
        let (dir, repo) = init();
        commit_file(&dir, &repo, "a.txt", "1\n", "c1");
        write(&dir, "a.txt", "stashed version\n");
        repo.stash_save("w", false).unwrap();
        write(&dir, "a.txt", "different local edit\n");

        assert!(repo.stash_pop(0).is_err());
        assert_eq!(repo.stash_list().unwrap().len(), 1); // not lost
        assert_eq!(read(&dir, "a.txt"), "different local edit\n"); // local edit untouched
    }
}
