/*
 * Copyright (c) Radical HQ Limited
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

use std::{
    ffi::OsStr,
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

use crate::{
    config::Config,
    error::{Error, Result, ResultExt},
    message::{MessageSection, MessageSectionsMap, build_commit_message, parse_message},
};
use git2::Oid;

#[derive(Debug, Clone)]
pub enum DryRunAction {
    Create {
        base: String,
        head: String,
        is_stacked: bool,
        draft: bool,
        reviewers: Vec<String>,
    },
    Update {
        pr_number: u64,
        base: String,
        head: String,
        is_stacked: bool,
    },
}

#[derive(Debug, Clone)]
pub struct PreparedCommit {
    pub oid: Oid,
    pub short_id: String,
    pub parent_oid: Oid,
    pub message: MessageSectionsMap,
    pub pull_request_number: Option<u64>,
    pub message_changed: bool,
    pub dry_run_action: Option<DryRunAction>,
    /// The other changes of the same Pull Request, bottom first. A Pull
    /// Request is a run of adjacent changes that link it; it is prepared as
    /// one commit whose `oid` and `message` are its top change's, and whose
    /// `parent_oid` is the parent of its bottom change. Empty for a Pull
    /// Request of one change.
    pub lower: Vec<PreparedCommit>,
}

impl PreparedCommit {
    /// The bottom change of the Pull Request.
    pub fn bottom_oid(&self) -> Oid {
        self.lower.first().map_or(self.oid, |c| c.oid)
    }

    /// Every change of the Pull Request, bottom first.
    pub fn member_oids(&self) -> impl Iterator<Item = Oid> + '_ {
        self.lower.iter().map(|c| c.oid).chain([self.oid])
    }
}

/// A mutable change that links a Pull Request in its description.
#[derive(Debug, Clone)]
pub struct LinkedChange {
    pub commit: Oid,
    pub change_id: String,
}

pub struct Jujutsu {
    repo_path: PathBuf,
    jj_bin: PathBuf,
    pub git_repo: git2::Repository,
}

impl Jujutsu {
    pub fn new(current_path: PathBuf) -> Result<Self> {
        let jj_bin = get_jj_bin();
        let workspace_root = discover_workspace_root(&jj_bin, &current_path)?;
        let git_repo = find_git_repo(&workspace_root)?;

        Ok(Self {
            repo_path: workspace_root,
            jj_bin,
            git_repo,
        })
    }

    pub fn git_command(&self) -> tokio::process::Command {
        let git_repo_path = self.git_repo.path();
        let mut cmd = tokio::process::Command::new("git");
        cmd.arg("--git-dir").arg(git_repo_path);
        cmd
    }

    pub fn get_prepared_commit_for_revision(
        &self,
        config: &Config,
        revision: &str,
    ) -> Result<PreparedCommit> {
        let commit_oid = self.resolve_revision_to_commit_id(revision)?;
        let commit = self.prepare_commit(config, commit_oid)?;
        let mut grouped = self.group_pull_requests(config, vec![commit])?;
        Ok(grouped.remove(0))
    }

    pub fn get_master_base_for_commit(&self, config: &Config, commit_oid: Oid) -> Result<Oid> {
        // Find the merge base between the commit and master.
        // Use git2 to resolve the ref directly rather than jj CLI, since
        // config.master_ref.local() returns a git ref path (e.g.
        // "refs/remotes/origin/main") which is not a valid jj revset.
        let master_oid = self.resolve_reference(config.master_ref.local())?;
        let merge_base = self.git_repo.merge_base(commit_oid, master_oid)?;
        Ok(merge_base)
    }

    pub fn get_prepared_commits_from_to(
        &self,
        config: &Config,
        from_revision: &str,
        to_revision: &str,
        is_inclusive: bool,
    ) -> Result<Vec<PreparedCommit>> {
        let operator = if is_inclusive { "::" } else { ".." };
        let commits =
            self.prepare_revset(config, &format!("{from_revision}{operator}{to_revision}"))?;
        self.group_pull_requests(config, commits)
    }

    /// The commits of `revset`, top first, as `jj log` lists them.
    pub fn commit_ids(&self, revset: &str) -> Result<Vec<Oid>> {
        let output = self.run([
            "log",
            "--no-graph",
            "-r",
            revset,
            "--template",
            "commit_id ++ \"\\n\"",
        ])?;
        output
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .map(|line| {
                Oid::from_str(line)
                    .map_err(|e| Error::new(format!("Failed to parse commit ID '{}': {}", line, e)))
            })
            .collect()
    }

    /// Prepare each commit of `revset` on its own, bottom first.
    fn prepare_revset(&self, config: &Config, revset: &str) -> Result<Vec<PreparedCommit>> {
        let mut commits = self
            .commit_ids(revset)?
            .into_iter()
            .map(|oid| self.prepare_commit(config, oid))
            .collect::<Result<Vec<_>>>()?;
        commits.reverse();
        Ok(commits)
    }

    /// Group `commits` (bottom first, each prepared on its own) into Pull
    /// Requests: each run of adjacent changes that link the same Pull
    /// Request becomes one `PreparedCommit` (see `PreparedCommit::lower`).
    /// A selection that includes part of a run is widened to the whole run.
    /// Changes that link one Pull Request but are not one run are refused.
    pub fn group_pull_requests(
        &self,
        config: &Config,
        mut commits: Vec<PreparedCommit>,
    ) -> Result<Vec<PreparedCommit>> {
        if commits.is_empty() {
            return Ok(commits);
        }
        for (number, linked) in self.linked_changes(config)? {
            if linked.len() < 2 {
                continue;
            }
            // Only a Pull Request the selection touches matters: one of its
            // changes is selected, or a selected change sits on one (it may
            // be between two of them). Changes elsewhere that link it
            // wrongly, like a `jj duplicate`, are refused only when selected.
            let linked_selected = linked
                .iter()
                .any(|l| commits.iter().any(|c| c.oid == l.commit));
            let on_linked = commits
                .iter()
                .any(|c| linked.iter().any(|l| l.commit == c.parent_oid));
            if !linked_selected && !on_linked {
                continue;
            }
            let run = match self.run_of(config, number, &linked) {
                Ok(run) => run,
                Err(error) if linked_selected => return Err(error),
                Err(_) => continue,
            };
            let in_run = |c: &PreparedCommit| run.iter().any(|r| r.oid == c.oid);
            if !commits.iter().any(in_run) {
                continue;
            }
            // Widen the selection at either end to the whole run.
            let first = run.iter().position(|r| r.oid == commits[0].oid);
            let last = run
                .iter()
                .position(|r| Some(r.oid) == commits.last().map(|c| c.oid));
            if let Some(first) = first {
                commits.splice(0..0, run[..first].iter().cloned());
            }
            if let Some(last) = last {
                commits.extend(run[last + 1..].iter().cloned());
            }
            if !run.iter().all(|r| commits.iter().any(|c| c.oid == r.oid)) {
                return Err(Error::new(format!(
                    "Pull Request #{number} is several changes, and only some of them \
                     are selected. Select all of them, or none."
                )));
            }
        }

        let grouped = join_runs(commits);
        let mut seen = std::collections::HashSet::new();
        for commit in &grouped {
            if let Some(number) = commit.pull_request_number
                && !seen.insert(number)
            {
                return Err(Error::new(format!(
                    "Pull Request #{number} is linked from changes that are not one run \
                     of adjacent changes. Keep the `Pull Request:` line only on the \
                     changes that make up the Pull Request, and remove it from the others \
                     (with `jj describe`); they get Pull Requests of their own."
                )));
            }
        }
        Ok(grouped)
    }

    /// The run of changes that make up Pull Request `number`, bottom first:
    /// the changes that link it, and any changes without a Pull Request
    /// between them. Refused unless that is one line of changes with no
    /// other Pull Request in it.
    fn run_of(
        &self,
        config: &Config,
        number: u64,
        linked: &[LinkedChange],
    ) -> Result<Vec<PreparedCommit>> {
        let ids = linked
            .iter()
            .map(|l| l.commit.to_string())
            .collect::<Vec<_>>()
            .join(" | ");
        let run = self.prepare_revset(config, &format!("roots({ids})::heads({ids})"))?;
        let is_line = run.windows(2).all(|pair| pair[1].parent_oid == pair[0].oid);
        let one_pull_request = run
            .iter()
            .all(|c| c.pull_request_number.is_none_or(|n| n == number));
        let has_all = linked.iter().all(|l| run.iter().any(|c| c.oid == l.commit));
        if is_line && one_pull_request && has_all {
            return Ok(run);
        }
        Err(Error::new(format!(
            "Pull Request #{number} is linked from changes that are not one run of \
             adjacent changes: {}. Keep the `Pull Request:` line only on the changes \
             that make up the Pull Request, and remove it from the others (with \
             `jj describe`); they get Pull Requests of their own.",
            linked
                .iter()
                .map(|l| short(&l.change_id))
                .collect::<Vec<_>>()
                .join(", ")
        )))
    }

    /// The mutable changes that link each Pull Request in a `Pull Request:`
    /// line.
    pub fn linked_changes(
        &self,
        config: &Config,
    ) -> Result<std::collections::BTreeMap<u64, Vec<LinkedChange>>> {
        let listing = self.run([
            "log",
            "--no-graph",
            "-r",
            "mutable() & description(substring-i:\"pull request:\")",
            "-T",
            "commit_id ++ \" \" ++ change_id ++ \"\\0\" ++ description ++ \"\\x1e\"",
        ])?;
        let mut linked: std::collections::BTreeMap<u64, Vec<LinkedChange>> = Default::default();
        for entry in listing.split('\x1e') {
            let Some((ids, description)) = entry.split_once('\0') else {
                continue;
            };
            let Some((commit, change_id)) = ids.trim().split_once(' ') else {
                continue;
            };
            let Ok(commit) = Oid::from_str(commit) else {
                continue;
            };
            let Some(number) = parse_message(description, MessageSection::Title)
                .get(&MessageSection::PullRequest)
                .and_then(|field| config.pull_request_number(field))
            else {
                continue;
            };
            linked.entry(number).or_default().push(LinkedChange {
                commit,
                change_id: change_id.to_string(),
            });
        }
        Ok(linked)
    }

    pub fn check_no_uncommitted_changes(&self) -> Result<()> {
        let output = self.run(["status"])?;

        // Check if there are any changes
        // Jujutsu reports "The working copy has no changes" when clean
        if output.trim().is_empty()
            || output.contains("No changes.")
            || output.contains("The working copy has no changes")
        {
            Ok(())
        } else {
            Err(Error::new(format!(
                "You have uncommitted changes:\n{}",
                output
            )))
        }
    }

    pub fn get_all_ref_names(&self) -> Result<std::collections::HashSet<String>> {
        // Use git for ref names since jj doesn't expose them directly
        let refs = self.git_repo.references()?;
        let mut ref_names = std::collections::HashSet::new();

        for reference in refs {
            let reference = reference?;
            if let Ok(name) = reference.name() {
                ref_names.insert(name.to_string());
            }
        }

        Ok(ref_names)
    }

    /// The branches on `remote` that match one of `patterns` (globs of full
    /// ref names, as `git ls-remote` takes them), with their heads, by
    /// branch name. One request to the remote.
    pub async fn remote_heads(
        &self,
        remote: &str,
        patterns: &[String],
    ) -> Result<std::collections::HashMap<String, Oid>> {
        let output = self
            .git_command()
            .args(["ls-remote", "--heads", "--", remote])
            .args(patterns)
            .stdin(Stdio::null())
            .output()
            .await?;
        if !output.status.success() {
            return Err(Error::new(format!(
                "git ls-remote failed: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            )));
        }
        String::from_utf8_lossy(&output.stdout)
            .lines()
            .filter_map(|line| {
                let (oid, name) = line.split_once('\t')?;
                Some((name.strip_prefix("refs/heads/")?.to_string(), oid))
            })
            .map(|(name, oid)| Ok((name, Oid::from_str(oid)?)))
            .collect()
    }

    pub fn resolve_reference(&self, ref_name: &str) -> Result<Oid> {
        let reference = self.git_repo.find_reference(ref_name)?;
        reference
            .target()
            .ok_or_else(|| Error::new(format!("Reference {} has no target", ref_name)))
    }

    pub fn get_tree_oid_for_commit(&self, commit_oid: Oid) -> Result<Oid> {
        let commit = self.git_repo.find_commit(commit_oid)?;
        Ok(commit.tree()?.id())
    }

    pub fn create_derived_commit(
        &self,
        original_commit_oid: Oid,
        message: &str,
        tree_oid: Oid,
        parent_oids: &[Oid],
    ) -> Result<Oid> {
        let original_commit = self.git_repo.find_commit(original_commit_oid)?;
        let tree = self.git_repo.find_tree(tree_oid)?;

        let mut parents = Vec::new();
        for &oid in parent_oids {
            parents.push(self.git_repo.find_commit(oid)?);
        }
        let parent_refs: Vec<_> = parents.iter().collect();

        // Take the user/email from the existing commit but make a new signature which has a
        // timestamp of now.
        let committer = git2::Signature::now(
            String::from_utf8_lossy(original_commit.committer().name_bytes()).as_ref(),
            String::from_utf8_lossy(original_commit.committer().email_bytes()).as_ref(),
        )?;

        // The author signature should reference the same user as the original commit, but we set
        // the timestamp to now, so this commit shows up in GitHub's timeline in the right place.
        let author = git2::Signature::now(
            String::from_utf8_lossy(original_commit.author().name_bytes()).as_ref(),
            String::from_utf8_lossy(original_commit.author().email_bytes()).as_ref(),
        )?;

        Ok(self
            .git_repo
            .commit(None, &author, &committer, message, &tree, &parent_refs)?)
    }

    /// Three-way merge of trees: `ours` with the changes from `base` to
    /// `theirs` applied. `None` when the merge conflicts.
    pub fn merge_trees(&self, base: Oid, ours: Oid, theirs: Oid) -> Result<Option<Oid>> {
        let repo = &self.git_repo;
        let mut merged = repo.merge_trees(
            &repo.find_tree(base)?,
            &repo.find_tree(ours)?,
            &repo.find_tree(theirs)?,
            None,
        )?;
        if merged.has_conflicts() {
            return Ok(None);
        }
        Ok(Some(merged.write_tree_to(repo)?))
    }

    /// The changes from `base` to `head` applied to `onto`, as a tree, or
    /// `None` when they conflict. For a change, `base` is its parent.
    pub fn cherry_pick_tree(&self, base: Oid, head: Oid, onto: Oid) -> Result<Option<Oid>> {
        self.merge_trees(
            self.get_tree_oid_for_commit(base)?,
            self.get_tree_oid_for_commit(onto)?,
            self.get_tree_oid_for_commit(head)?,
        )
    }

    /// Write changed commit messages back with `jj describe`. The other
    /// changes of a Pull Request get the top change's `Pull Request:` line,
    /// or lose theirs when it has none (after `jj spr close`).
    pub fn rewrite_commit_messages(
        &self,
        config: &Config,
        commits: &mut [PreparedCommit],
    ) -> Result<()> {
        for prepared_commit in commits.iter_mut() {
            let line = prepared_commit
                .message
                .get(&MessageSection::PullRequest)
                .cloned();
            let number = line
                .as_deref()
                .and_then(|field| config.pull_request_number(field));
            for member in prepared_commit.lower.iter_mut() {
                if member.pull_request_number != number {
                    match &line {
                        Some(line) => member
                            .message
                            .insert(MessageSection::PullRequest, line.clone()),
                        None => member.message.remove(&MessageSection::PullRequest),
                    };
                    member.pull_request_number = number;
                    member.message_changed = true;
                }
                if member.message_changed {
                    self.describe(member)?;
                }
            }
            if prepared_commit.message_changed {
                self.describe(prepared_commit)?;
            }
        }
        Ok(())
    }

    /// `jj describe` a commit with its prepared message.
    fn describe(&self, prepared_commit: &mut PreparedCommit) -> Result<()> {
        let new_message = build_commit_message(&prepared_commit.message);
        let change_id = self.get_change_id_for_commit(prepared_commit.oid)?;

        let mut cmd = Command::new(&self.jj_bin);
        cmd.args(["describe", "-r", &change_id, "-m", &new_message])
            .current_dir(&self.repo_path)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        let output = cmd.output()?;
        if !output.status.success() {
            return Err(Error::new(format!(
                "Failed to update commit message: {}",
                String::from_utf8_lossy(&output.stderr)
            )));
        }
        prepared_commit.message_changed = false;
        Ok(())
    }

    pub fn prepare_commit(&self, config: &Config, commit_oid: Oid) -> Result<PreparedCommit> {
        let commit = self.git_repo.find_commit(commit_oid)?;
        let short_id = format!("{:.7}", commit_oid);

        let parent_oid = if commit.parents().count() > 0 {
            commit.parent(0)?.id()
        } else {
            // For initial commit, use a null OID or the commit itself
            commit_oid
        };

        let message_text = commit.message().unwrap_or("").to_string();
        let message = parse_message(&message_text, MessageSection::Title);

        let pull_request_number = message
            .get(&MessageSection::PullRequest)
            .and_then(|field| config.pull_request_number(field));

        Ok(PreparedCommit {
            oid: commit_oid,
            short_id,
            parent_oid,
            message,
            pull_request_number,
            message_changed: false,
            dry_run_action: None,
            lower: Vec::new(),
        })
    }

    fn resolve_revision_to_commit_id(&self, revision: &str) -> Result<Oid> {
        let output = self.run([
            "log",
            "--no-graph",
            "-r",
            revision,
            "--template",
            "commit_id",
        ])?;

        let commit_id_str = output.trim();
        Oid::from_str(commit_id_str).map_err(|e| {
            Error::new(format!(
                "Failed to parse commit ID '{}' from jj output: {}",
                commit_id_str, e
            ))
        })
    }

    fn get_change_id_for_commit(&self, commit_oid: Oid) -> Result<String> {
        // Get the change ID for a given commit OID
        let output = self.run([
            "log",
            "--no-graph",
            "-r",
            &commit_oid.to_string(),
            "--template",
            "change_id",
        ])?;

        Ok(output.trim().to_string())
    }

    /// Require `commits` (bottom first) to be one straight line of changes,
    /// each with a single parent. `what` names the command, for the error.
    pub fn require_linear(&self, commits: &[PreparedCommit], what: &str) -> Result<()> {
        for (i, commit) in commits.iter().enumerate() {
            // Every change of a Pull Request, not only its top.
            for member in commit.lower.iter().chain([commit]) {
                if self.git_repo.find_commit(member.oid)?.parent_count() != 1 {
                    return Err(Error::new(format!(
                        "Not {what}: the changes up to the target are not a single \
                         line of changes ({} is a merge). Pass `-r <top of one \
                         stack>` to pick one stack.",
                        member.short_id,
                    )));
                }
            }
            if i > 0 && commit.parent_oid != commits[i - 1].oid {
                return Err(Error::new(format!(
                    "Not {what}: the changes up to the target are not a single line of \
                     changes ({} is on a separate branch). Pass `-r <top of one \
                     stack>` to pick one stack.",
                    // The bottom change is the one whose parent is elsewhere.
                    commit.lower.first().unwrap_or(commit).short_id,
                )));
            }
        }
        Ok(())
    }

    /// The tree of `commit`, if the commit is in the local repository.
    pub fn tree_if_present(&self, commit: Oid) -> Option<Oid> {
        if let Ok(commit) = self.git_repo.find_commit(commit) {
            return Some(commit.tree_id());
        }
        // Objects fetched since the repository was opened may be in a new pack.
        if let Ok(odb) = self.git_repo.odb() {
            let _ = odb.refresh();
        }
        self.git_repo.find_commit(commit).ok().map(|c| c.tree_id())
    }

    /// Whether `tree` already contains the changes from `from` to `to` (all
    /// trees): applying them to `tree` with a three-way merge changes nothing
    /// and does not conflict. Unlike comparing trees, this still holds when
    /// `tree` also has other changes, such as a newer main branch.
    pub fn tree_contains_diff(&self, tree: Oid, from: Oid, to: Oid) -> Result<bool> {
        Ok(self.merge_trees(from, tree, to)? == Some(tree))
    }

    /// Where a Pull Request branched off its base.
    pub fn fork_point(&self, config: &Config, pr: &crate::github::PullRequest) -> Result<Oid> {
        let base = if pr.base_oid.is_zero() {
            self.resolve_reference(config.master_ref.local())?
        } else {
            pr.base_oid
        };
        Ok(self.git_repo.merge_base(pr.head_oid, base)?)
    }

    /// The newer commit of the main branch that a Pull Request's head is
    /// based on, if GitHub moved it there and `local_base` (the parent of the
    /// local change) is older. That happens when someone presses the stack's
    /// rebase button or "Update branch" in GitHub's web UI. It is `None` when
    /// only the main branch moved, so that a stack is never rebased merely
    /// because main has new commits.
    pub fn newer_main_base(
        &self,
        config: &Config,
        local_base: Oid,
        remote_head: Oid,
    ) -> Result<Option<Oid>> {
        if remote_head.is_zero() || self.tree_if_present(remote_head).is_none() {
            return Ok(None);
        }
        let main = self.resolve_reference(config.master_ref.local())?;
        let remote_base = match self.git_repo.merge_base(remote_head, main) {
            Ok(base) => base,
            // No common history: GitHub did not move it onto main.
            Err(e) if e.code() == git2::ErrorCode::NotFound => return Ok(None),
            Err(e) => return Err(e.into()),
        };
        if remote_base != local_base
            && self.git_repo.graph_descendant_of(remote_base, local_base)?
        {
            Ok(Some(remote_base))
        } else {
            Ok(None)
        }
    }

    /// Whether the Pull Request has edits that its local change lacks, so
    /// that pushing the local change would drop them.
    ///
    /// Edits made locally since the last push must not count, so this is not
    /// a plain comparison. The Pull Request is fine if the local change
    /// contains its diff, or if its diff is that of a version of the local
    /// change from `jj evolog` (the one last pushed; after a stacked merge
    /// GitHub rebases that onto a new base, so the trees can differ).
    ///
    /// It is also fine if the local parent contains the diff. GitHub's stack
    /// rebase can leave a branch holding only the content of the Pull
    /// Request below it, which the local change need not contain (it may
    /// undo some of it), but which pushing does not lose.
    pub fn pr_has_remote_changes(
        &self,
        config: &Config,
        local: &PreparedCommit,
        pr: &crate::github::PullRequest,
    ) -> Result<bool> {
        let Some(head_tree) = (!pr.head_oid.is_zero())
            .then(|| self.tree_if_present(pr.head_oid))
            .flatten()
        else {
            return Ok(false);
        };
        let fork_tree = self.get_tree_oid_for_commit(self.fork_point(config, pr)?)?;

        let local_tree = self.get_tree_oid_for_commit(local.oid)?;
        if self.tree_contains_diff(local_tree, fork_tree, head_tree)? {
            return Ok(false);
        }
        let parent_tree = self.get_tree_oid_for_commit(local.parent_oid)?;
        if self.tree_contains_diff(parent_tree, fork_tree, head_tree)? {
            return Ok(false);
        }
        for version in self.evolog(local.oid)? {
            let commit = self.git_repo.find_commit(version)?;
            let tree = commit.tree_id();
            if tree == head_tree {
                return Ok(false);
            }
            let Ok(parent) = commit.parent(0) else {
                continue;
            };
            if self.tree_contains_diff(head_tree, parent.tree_id(), tree)?
                && self.tree_contains_diff(tree, fork_tree, head_tree)?
            {
                return Ok(false);
            }
        }
        Ok(true)
    }

    /// The conflicted changes in `revset`, one per line as
    /// `<short change ID> <title>`, for reporting. Empty when there are none.
    pub fn conflicted_changes(&self, revset: &str) -> Result<String> {
        Ok(self
            .run([
                "log",
                "--no-graph",
                "-r",
                &format!("conflicts() & ({revset})"),
                "-T",
                "change_id.short() ++ \" \" ++ description.first_line() ++ \"\\n\"",
            ])?
            .trim_end()
            .to_string())
    }

    /// Every version of the change that `commit` is, newest first, from
    /// `jj evolog`, leaving out any that are not in the Git repository.
    pub fn evolog(&self, commit: Oid) -> Result<Vec<Oid>> {
        let listing = self.run([
            "evolog",
            "--no-graph",
            "-r",
            &commit.to_string(),
            "-T",
            "commit.commit_id() ++ \"\\n\"",
        ])?;
        Ok(listing
            .lines()
            .filter_map(|line| Oid::from_str(line.trim()).ok())
            .filter(|oid| self.tree_if_present(*oid).is_some())
            .collect())
    }

    /// Map each commit to its change ID with one jj invocation.
    pub fn change_ids_for(
        &self,
        commits: &[PreparedCommit],
    ) -> Result<std::collections::HashMap<Oid, String>> {
        if commits.is_empty() {
            return Ok(Default::default());
        }
        let revset = commits
            .iter()
            .flat_map(|c| c.member_oids())
            .map(|oid| oid.to_string())
            .collect::<Vec<_>>()
            .join(" | ");
        let listing = self.run([
            "log",
            "--no-graph",
            "-r",
            &revset,
            "-T",
            "commit_id ++ \" \" ++ change_id ++ \"\\n\"",
        ])?;
        Ok(listing
            .lines()
            .filter_map(|line| {
                let (commit, change) = line.split_once(' ')?;
                Some((Oid::from_str(commit).ok()?, change.trim().to_string()))
            })
            .collect())
    }

    /// Run jj in the workspace and return its standard output. Its standard
    /// error (progress and notices) goes to the terminal.
    pub fn run<I, S>(&self, args: I) -> Result<String>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let mut command = Command::new(&self.jj_bin);
        command.args(args);
        command.current_dir(&self.repo_path);
        command.stdout(Stdio::piped());

        let child = command.spawn().context("jj failed to spawn".to_string())?;
        let output = child
            .wait_with_output()
            .context("failed to wait for jj to exit".to_string())?;

        if output.status.success() {
            let output = String::from_utf8(output.stdout)
                .context("jujutsu output was not valid UTF-8".to_string())?;
            Ok(output)
        } else {
            Err(Error::new(format!(
                "jujutsu exited with code {}, stderr:\n{}",
                output
                    .status
                    .code()
                    .map_or_else(|| "(unknown)".to_string(), |c| c.to_string()),
                String::from_utf8_lossy(&output.stderr)
            )))
        }
    }
}

/// Cut a selection of Pull Requests (bottom first, as `group_pull_requests`
/// returns it) after each one whose top change is in `tips`, and join each
/// part into one Pull Request, for `jj spr diff --tips` and `--one-pr`.
/// Every tip has to be the top change of a selected Pull Request. Changes
/// above the topmost tip are left as they are.
pub fn join_at_tips(commits: Vec<PreparedCommit>, tips: &[Oid]) -> Result<Vec<PreparedCommit>> {
    for &tip in tips {
        if commits.iter().any(|c| c.oid == tip) {
            continue;
        }
        let Some(pr) = commits
            .iter()
            .find(|c| c.lower.iter().any(|m| m.oid == tip))
        else {
            return Err(Error::new(format!(
                "Not submitting: tip {tip:.7} is not in the range from the base up to \
                 the topmost tip."
            )));
        };
        return Err(Error::new(format!(
            "Not submitting: tip {tip:.7} is below the top change of {}, and a Pull \
             Request can't be split this way. List its top change, {}, instead.",
            pr.pull_request_number.map_or_else(
                || "a Pull Request".to_string(),
                |n| format!("Pull Request #{n}")
            ),
            pr.short_id,
        )));
    }
    let mut joined = Vec::new();
    let mut part = Vec::new();
    for commit in commits {
        let is_tip = tips.contains(&commit.oid);
        part.push(commit);
        if is_tip {
            joined.push(join_into_one(std::mem::take(&mut part))?);
        }
    }
    joined.append(&mut part);
    Ok(joined)
}

/// Join a selection of Pull Requests (bottom first, a line of changes) into
/// one. It is the Pull Request one of them already links, if any; the
/// changes without its `Pull Request:` line get it when the messages are
/// rewritten.
fn join_into_one(mut commits: Vec<PreparedCommit>) -> Result<PreparedCommit> {
    let top_id = commits
        .last()
        .expect("one commit at least")
        .short_id
        .clone();
    let mut numbers: Vec<u64> = commits
        .iter()
        .filter_map(|c| c.pull_request_number)
        .collect();
    numbers.dedup();
    if numbers.len() > 1 {
        return Err(Error::new(format!(
            "Not submitting: the changes up to {top_id} would become one Pull \
             Request, but they link {}. To keep those apart, list the top change \
             of every Pull Request in the range with --tips.",
            numbers
                .iter()
                .map(|n| format!("#{n}"))
                .collect::<Vec<_>>()
                .join(", ")
        )));
    }
    if !commits
        .windows(2)
        .all(|pair| pair[1].parent_oid == pair[0].oid)
    {
        return Err(Error::new(format!(
            "Not submitting: the changes up to {top_id} are not a line of changes, \
             each the parent of the next, so they can't become one Pull Request."
        )));
    }
    let link = commits.iter().find_map(|c| {
        let number = c.pull_request_number?;
        Some((number, c.message.get(&MessageSection::PullRequest)?.clone()))
    });
    let base = commits[0].parent_oid;
    let mut top = commits.pop().expect("one commit at least");
    let mut lower = Vec::new();
    for mut commit in commits {
        lower.append(&mut commit.lower);
        lower.push(commit);
    }
    lower.append(&mut top.lower);
    if top.pull_request_number.is_none()
        && let Some((number, link)) = link
    {
        top.message.insert(MessageSection::PullRequest, link);
        top.pull_request_number = Some(number);
        top.message_changed = true;
    }
    top.parent_oid = base;
    top.lower = lower;
    Ok(top)
}

/// Join each run of adjacent changes (bottom first) that link the same Pull
/// Request into one `PreparedCommit`. A change without a Pull Request
/// between two changes of the same one joins it.
fn join_runs(commits: Vec<PreparedCommit>) -> Vec<PreparedCommit> {
    let mut joined: Vec<PreparedCommit> = Vec::new();
    // Changes without a Pull Request, after the last one that has one.
    let mut unlinked: Vec<PreparedCommit> = Vec::new();
    for commit in commits {
        let Some(number) = commit.pull_request_number else {
            unlinked.push(commit);
            continue;
        };
        let continues_run = joined.last().is_some_and(|last| {
            let chain = std::iter::once(last.oid).chain(unlinked.iter().map(|c| c.oid));
            let parents = unlinked
                .iter()
                .map(|c| c.parent_oid)
                .chain([commit.parent_oid]);
            last.pull_request_number == Some(number) && chain.eq(parents)
        });
        if continues_run {
            let mut run = joined.pop().expect("checked above");
            let base = run.parent_oid;
            let mut lower = std::mem::take(&mut run.lower);
            lower.push(run);
            lower.append(&mut unlinked);
            let mut top = commit;
            top.parent_oid = base;
            top.lower = lower;
            joined.push(top);
        } else {
            joined.append(&mut unlinked);
            joined.push(commit);
        }
    }
    joined.append(&mut unlinked);
    joined
}

/// The first eight characters of a change ID, for messages.
pub fn short(change_id: &str) -> &str {
    &change_id[..change_id.len().min(8)]
}

fn get_jj_bin() -> PathBuf {
    std::env::var_os("JJ").map_or_else(|| "jj".into(), |v| v.into())
}

/// Discover the Jujutsu workspace root from the given directory by running `jj root`.
fn discover_workspace_root(jj_bin: &PathBuf, current_dir: &Path) -> Result<PathBuf> {
    let output = Command::new(jj_bin)
        .arg("root")
        .current_dir(current_dir)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .context("jj failed to spawn".to_string())?;

    if output.status.success() {
        let root = String::from_utf8(output.stdout)
            .context("jj root output was not valid UTF-8".to_string())?;
        Ok(PathBuf::from(root.trim()))
    } else {
        Err(Error::new(
            "This command requires a Jujutsu repository. \
                 Could not find a Jujutsu workspace in the current directory."
                .to_string(),
        ))
    }
}

/// Resolve the `.jj/repo` path, handling the indirection used by non-default
/// workspaces where `.jj/repo` is a file containing a relative path to the
/// primary workspace's repo directory.
fn resolve_repo_dir(workspace_root: &Path) -> Result<PathBuf> {
    let repo_path = workspace_root.join(".jj").join("repo");
    if repo_path.is_file() {
        // Non-default workspace: .jj/repo is a file whose contents are a
        // path (relative to .jj/) pointing to the actual repo directory.
        let contents = std::fs::read(&repo_path)
            .context("failed to read .jj/repo pointer file".to_string())?;
        let relative = String::from_utf8(contents)
            .context(".jj/repo pointer was not valid UTF-8".to_string())?;
        let jj_dir = workspace_root.join(".jj");
        jj_dir
            .join(relative.trim())
            .canonicalize()
            .context("failed to resolve .jj/repo pointer".to_string())
    } else {
        Ok(repo_path)
    }
}

/// Find the git2::Repository backing a Jujutsu workspace.
///
/// Supports colocated repos (`.git` at the workspace root), non-colocated
/// repos (git backend inside `.jj/repo/store/`), and non-default workspaces
/// (where `.jj/repo` is a pointer file).
fn find_git_repo(workspace_root: &Path) -> Result<git2::Repository> {
    // First try colocated: .git at the root
    let dot_git = workspace_root.join(".git");
    if dot_git.exists() {
        return Ok(git2::Repository::open(workspace_root)?);
    }

    // Resolve .jj/repo (may be a file in non-default workspaces)
    let repo_dir = resolve_repo_dir(workspace_root)?;
    let store_dir = repo_dir.join("store");
    let git_target_path = store_dir.join("git_target");
    if git_target_path.exists() {
        let git_target = std::fs::read_to_string(&git_target_path)
            .context("failed to read .jj/repo/store/git_target".to_string())?;
        let git_path = git_target.trim();
        let git_path = if Path::new(git_path).is_absolute() {
            PathBuf::from(git_path)
        } else {
            store_dir.join(git_path).canonicalize()?
        };
        return Ok(git2::Repository::open(&git_path)?);
    }

    Err(Error::new(
        "Could not find Git backend for Jujutsu repository. \
         Ensure you have a Jujutsu repository with a Git backend."
            .to_string(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{fs, path::Path};
    use tempfile::TempDir;

    fn create_test_config() -> Config {
        Config::new(
            "test_owner".into(),
            "test_repo".into(),
            "origin".into(),
            "main".into(),
            "spr/test/".into(),
            false,
        )
    }

    fn create_jujutsu_test_repo() -> (TempDir, PathBuf) {
        let temp_dir = TempDir::new().expect("Failed to create temp directory");
        let repo_path = temp_dir.path().to_path_buf();

        // Initialize a Jujutsu repository
        let output = std::process::Command::new("jj")
            .args(["git", "init", "--colocate"])
            .current_dir(&repo_path)
            .output()
            .expect("Failed to run jj git init");

        if !output.status.success() {
            panic!(
                "Failed to initialize jj repo: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }

        // Set up basic jj config
        let _ = std::process::Command::new("jj")
            .args(["config", "set", "--repo", "user.name", "Test User"])
            .current_dir(&repo_path)
            .output();

        let _ = std::process::Command::new("jj")
            .args(["config", "set", "--repo", "user.email", "test@example.com"])
            .current_dir(&repo_path)
            .output();

        (temp_dir, repo_path)
    }

    fn create_jujutsu_commit(repo_path: &Path, message: &str, file_content: &str) -> String {
        // Create a file
        let file_path = repo_path.join("test.txt");
        fs::write(&file_path, file_content).expect("Failed to write test file");

        // Create a commit using jj
        let output = std::process::Command::new("jj")
            .args(["commit", "-m", message])
            .current_dir(repo_path)
            .output()
            .expect("Failed to run jj commit");

        if !output.status.success() {
            panic!(
                "Failed to create jj commit: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }

        // Get the change ID of the created commit
        let output = std::process::Command::new("jj")
            .args(["log", "--no-graph", "-r", "@-", "--template", "change_id"])
            .current_dir(repo_path)
            .output()
            .expect("Failed to get change ID");

        String::from_utf8(output.stdout)
            .expect("Invalid UTF-8 in jj output")
            .trim()
            .to_string()
    }

    #[test]
    fn test_jujutsu_creation() {
        let (_temp_dir, repo_path) = create_jujutsu_test_repo();
        let jj = Jujutsu::new(repo_path.clone()).expect("Failed to create Jujutsu instance");
        assert!(jj.repo_path.exists());
        assert!(jj.repo_path.join(".jj").exists());
    }

    #[test]
    fn test_revision_resolution() {
        let (_temp_dir, repo_path) = create_jujutsu_test_repo();
        let config = create_test_config();

        // Create some commits
        let _commit1 = create_jujutsu_commit(&repo_path, "First commit", "content1");
        let _commit2 = create_jujutsu_commit(&repo_path, "Second commit", "content2");

        let jj = Jujutsu::new(repo_path.clone()).expect("Failed to create Jujutsu instance");

        // Test resolving current revision (@)
        let result = jj.get_prepared_commit_for_revision(&config, "@");
        assert!(
            result.is_ok(),
            "Failed to resolve @ revision: {:?}",
            result.err()
        );

        // Test resolving previous revision (@-)
        let result = jj.get_prepared_commit_for_revision(&config, "@-");
        assert!(
            result.is_ok(),
            "Failed to resolve @- revision: {:?}",
            result.err()
        );
    }

    #[test]
    fn test_commit_range() {
        let (_temp_dir, repo_path) = create_jujutsu_test_repo();
        let config = create_test_config();

        // Create multiple commits
        let _commit1 = create_jujutsu_commit(&repo_path, "First commit", "content1");
        let _commit2 = create_jujutsu_commit(&repo_path, "Second commit", "content2");
        let _commit3 = create_jujutsu_commit(&repo_path, "Third commit", "content3");

        let jj = Jujutsu::new(repo_path.clone()).expect("Failed to create Jujutsu instance");

        // Test getting commit range
        let result = jj.get_prepared_commits_from_to(&config, "@----", "@-", false);
        assert!(
            result.is_ok(),
            "Failed to get commit range: {:?}",
            result.err()
        );

        if let Ok(commits) = result {
            // Should get 3 commits in the range
            assert_eq!(commits.len(), 3, "Should get exactly 3 commits in range");

            // Commits must be in bottom-to-top order (oldest to newest).
            let first_commit_title = commits[0]
                .message
                .get(&MessageSection::Title)
                .expect("First commit should have a title");
            let last_commit_title = commits[2]
                .message
                .get(&MessageSection::Title)
                .expect("Last commit should have a title");

            assert!(
                first_commit_title.contains("First commit"),
                "First element should be the oldest commit 'First commit', got: {}",
                first_commit_title
            );
            assert!(
                last_commit_title.contains("Third commit"),
                "Last element should be the newest commit 'Third commit', got: {}",
                last_commit_title
            );
        }
    }

    #[test]
    fn test_status_check() {
        let (_temp_dir, repo_path) = create_jujutsu_test_repo();

        let jj = Jujutsu::new(repo_path.clone()).expect("Failed to create Jujutsu instance");

        // Should pass since new repo has no changes
        let result = jj.check_no_uncommitted_changes();
        assert!(
            result.is_ok(),
            "Status check should pass for clean repo: {:?}",
            result.err()
        );
    }

    #[test]
    fn test_derived_commit_has_different_timestamp() {
        let (_temp_dir, repo_path) = create_jujutsu_test_repo();

        // Create a commit with some content
        let _commit1 = create_jujutsu_commit(&repo_path, "Original commit", "original content");

        let jj = Jujutsu::new(repo_path.clone()).expect("Failed to create Jujutsu instance");

        // Get the original commit
        let original_commit_oid = jj
            .resolve_revision_to_commit_id("@-")
            .expect("Failed to resolve @- revision");
        let original_commit = jj
            .git_repo
            .find_commit(original_commit_oid)
            .expect("Failed to find original commit");

        // Sleep briefly to ensure timestamp difference
        std::thread::sleep(std::time::Duration::from_secs(1));

        // Create a derived commit
        let tree_oid = original_commit.tree().expect("Failed to get tree").id();
        let parent_oids = if original_commit.parents().count() > 0 {
            vec![
                original_commit
                    .parent(0)
                    .expect("Failed to get parent")
                    .id(),
            ]
        } else {
            vec![]
        };

        let derived_commit_oid = jj
            .create_derived_commit(
                original_commit_oid,
                "Derived commit message",
                tree_oid,
                &parent_oids,
            )
            .expect("Failed to create derived commit");

        // Get the derived commit
        let derived_commit = jj
            .git_repo
            .find_commit(derived_commit_oid)
            .expect("Failed to find derived commit");

        // Verify that derived timestamps are newer than original
        let original_author_time = original_commit.author().when();
        let derived_author_time = derived_commit.author().when();
        let original_committer_time = original_commit.committer().when();
        let derived_committer_time = derived_commit.committer().when();

        assert!(
            derived_author_time.seconds() > original_author_time.seconds(),
            "Derived commit author timestamp should be newer than original"
        );

        assert!(
            derived_committer_time.seconds() > original_committer_time.seconds(),
            "Derived commit committer timestamp should be newer than original"
        );
    }
}
