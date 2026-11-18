/*
 * Copyright (c) Radical HQ Limited
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

use std::{collections::HashSet, iter::zip};

use crate::{
    error::{Error, Result, ResultExt, add_error},
    github::{
        GitHub, PullRequest, PullRequestRequestReviewers, PullRequestState, PullRequestUpdate,
    },
    message::{MessageSection, build_github_body, build_message, validate_commit_message},
    output::{output, write_commit_title},
    utils::{parse_name_list, remove_all_parens, run_command},
};
use git2::Oid;
use indoc::formatdoc;

#[derive(Debug, clap::Parser)]
pub struct DiffOptions {
    /// Create/update pull requests for commits in range from base to revision
    #[clap(long, short = 'a')]
    all: bool,

    /// Update the pull request title and description on GitHub from the local
    /// commit message
    #[clap(long)]
    update_message: bool,

    /// Title for the Pull Request on GitHub, in place of the top change's.
    /// It replaces the title of an existing Pull Request. Local descriptions
    /// are not changed. Only for a run with one Pull Request.
    #[clap(long, value_name = "TEXT", value_parser = non_blank, conflicts_with = "update_message")]
    title: Option<String>,

    /// Body for the Pull Request on GitHub, in place of the top change's
    /// description. It replaces the body of an existing Pull Request, and an
    /// empty one means an empty body. Local descriptions are not changed.
    /// Only for a run with one Pull Request.
    #[clap(long, value_name = "TEXT", conflicts_with = "update_message")]
    body: Option<String>,

    /// Submit any new Pull Request as a draft
    #[clap(long)]
    draft: bool,

    /// Message to be used for commits updating existing pull requests (e.g.
    /// 'rebase' or 'review comments')
    #[clap(long, short = 'm')]
    message: Option<String>,

    /// Submit this commit as if it was cherry-picked on master. Do not base it
    /// on any intermediate changes between the master branch and this commit.
    #[clap(long)]
    cherry_pick: bool,

    /// Remove the "Cherry Pick:" marker from the commit description and ignore
    /// it for this invocation. Future `jj spr diff` invocations will behave
    /// as though --cherry-pick was not specified.
    #[clap(long, conflicts_with = "cherry_pick")]
    no_cherry_pick: bool,

    /// Base revision for --all mode (if not specified, uses trunk)
    #[clap(long)]
    base: Option<String>,

    /// Jujutsu revision(s) to operate on. Can be a single revision like '@' or a range like 'main..@' or 'a::c'.
    /// If a range is provided, behaves like --all mode. If not specified, uses '@-'.
    #[clap(short = 'r', long)]
    revision: Option<String>,

    /// Preview what would happen without pushing or creating PRs
    #[clap(long)]
    pub dry_run: bool,

    /// Push even if a Pull Request has commits that are not in its local
    /// change (someone else pushed to it). Their edits stay in the branch
    /// history but disappear from the Pull Request's diff.
    #[clap(long)]
    discard_remote_changes: bool,

    /// Submit the selected range of changes as one Pull Request, with a
    /// commit for each change. The changes stay separate locally, and each
    /// gets the Pull Request's `Pull Request:` line. A new Pull Request takes
    /// its title and message from the top change.
    #[clap(long)]
    one_pr: bool,

    /// Submit the changes from the base up to the topmost of these as one
    /// Pull Request per tip, each made of the changes above the tip below it
    /// and up to its own, as with --one-pr. The tips have to be in a line.
    /// Each Pull Request already in the range needs a tip of its own: its
    /// top change, or a new change above it.
    #[clap(long, value_name = "REVSET", conflicts_with_all = ["revision", "all", "one_pr"])]
    tips: Option<String>,
}

fn non_blank(text: &str) -> std::result::Result<String, String> {
    let text = text.trim();
    if text.is_empty() {
        return Err("it is empty".to_string());
    }
    Ok(text.to_string())
}

impl DiffOptions {
    fn push_options(&self) -> PushOptions {
        PushOptions {
            update_message: self.update_message,
            draft: self.draft,
            message: self.message.clone(),
            cherry_pick: self.cherry_pick,
            no_cherry_pick: self.no_cherry_pick,
            dry_run: self.dry_run,
            discard_remote_changes: self.discard_remote_changes,
        }
    }
}

/// The options `publish` takes from the command line, for `diff` and
/// `submit`.
#[derive(Debug, Default)]
pub(crate) struct PushOptions {
    pub update_message: bool,
    pub draft: bool,
    /// The message for commits that update a Pull Request. Without it,
    /// `publish` asks for one.
    pub message: Option<String>,
    pub cherry_pick: bool,
    pub no_cherry_pick: bool,
    pub dry_run: bool,
    pub discard_remote_changes: bool,
}

/// What a front end sets for one Pull Request in place of what `publish`
/// works out from the top change's description: the title and body the Pull
/// Request should have on GitHub, and the branch for a new one.
#[derive(Debug, Clone, Default)]
pub(crate) struct Overrides {
    pub title: Option<String>,
    pub body: Option<String>,
    pub branch: Option<String>,
}

/// Where `publish` records what it did.
pub(crate) trait Record {
    /// The new Pull Request at `index` is about to be pushed to `branch` for
    /// the first time.
    fn pushing(&mut self, _index: usize, _branch: &str) -> Result<()> {
        Ok(())
    }

    /// The Pull Request at `index` was published: created, updated, or
    /// found up to date. `number` is `None` for one that a dry run would
    /// create.
    fn published(
        &mut self,
        index: usize,
        commit: &mut crate::jj::PreparedCommit,
        number: Option<u64>,
    ) -> Result<()>;

    /// Publishing finished, or stopped at an error.
    fn finish(&mut self, commits: &mut [crate::jj::PreparedCommit]) -> Result<()>;
}

/// `diff`'s record: a `Pull Request:` line in each change of a new Pull
/// Request, written with `jj describe` at the end of the run.
struct Descriptions<'a> {
    jj: &'a crate::jj::Jujutsu,
    config: &'a crate::config::Config,
    dry_run: bool,
}

impl Record for Descriptions<'_> {
    fn published(
        &mut self,
        _index: usize,
        commit: &mut crate::jj::PreparedCommit,
        number: Option<u64>,
    ) -> Result<()> {
        if let Some(number) = number
            && commit.pull_request_number.is_none()
        {
            commit.message.insert(
                MessageSection::PullRequest,
                self.config.pull_request_url(number),
            );
            commit.message_changed = true;
        }
        Ok(())
    }

    fn finish(&mut self, commits: &mut [crate::jj::PreparedCommit]) -> Result<()> {
        if self.dry_run {
            return Ok(());
        }
        self.jj.rewrite_commit_messages(self.config, commits)
    }
}

/// The Pull Request branch of a change's parent, which the change's Pull
/// Request targets.
#[derive(Debug, Clone)]
struct ParentPullRequest {
    number: Option<u64>,
    branch: crate::github::GitHubBranch,
    head_oid: Oid,
}

/// What `diff_impl` published (or would publish, in a dry run) for one change.
#[derive(Debug, Clone)]
struct Published {
    number: Option<u64>,
    head: crate::github::GitHubBranch,
    head_oid: Oid,
    /// The base branch, when this run set or confirmed it. `None` for a
    /// parent Pull Request that was only looked up.
    base: Option<String>,
}

/// What `diff_impl` needs for one change that does not depend on what this
/// run publishes (see `resolve_change`).
#[derive(Debug)]
struct Resolved {
    /// The commit message with the cherry-pick marker and the reviewers as
    /// they are to be stored locally.
    message: crate::message::MessageSectionsMap,
    message_changed: bool,
    cherry_pick: bool,
    /// The trees the Pull Request branch and its base should have.
    head_tree: Oid,
    base_tree: Oid,
    /// The reviewers to request for a new Pull Request.
    reviewers: PullRequestRequestReviewers,
}

/// The changes of a `--tips` revset and the topmost of them. They have to be
/// in a line, each an ancestor of the topmost.
fn resolve_tips(jj: &crate::jj::Jujutsu, revset: &str) -> Result<(Oid, Vec<Oid>)> {
    let tips = jj.commit_ids(revset)?;
    match jj.commit_ids(&format!("heads({revset})"))?[..] {
        [topmost] => Ok((topmost, tips)),
        [] => Err(Error::new(format!(
            "Not submitting: --tips '{revset}' has no changes."
        ))),
        ref heads => Err(Error::new(format!(
            "Not submitting: the tips have to be in a line, each an ancestor of the \
             topmost, but {} are on separate branches.",
            heads
                .iter()
                .map(|oid| format!("{oid:.7}"))
                .collect::<Vec<_>>()
                .join(", ")
        ))),
    }
}

/// `ancestor` is `descendant` or one of its ancestors.
fn is_ancestor(jj: &crate::jj::Jujutsu, ancestor: Oid, descendant: Oid) -> Result<bool> {
    Ok(ancestor == descendant || jj.git_repo.graph_descendant_of(descendant, ancestor)?)
}

/// The tree that GitHub's rebase of a stacked Pull Request would give the
/// branch at `head` on a base at `base`: the branch's non-merge commits that
/// are not in `base`, oldest first, replayed onto `base`. `None` when one of
/// them conflicts.
fn replayed_tree(jj: &crate::jj::Jujutsu, head: Oid, base: Oid) -> Result<Option<Oid>> {
    let repo = &jj.git_repo;
    let mut walk = repo.revwalk()?;
    walk.set_sorting(git2::Sort::TOPOLOGICAL | git2::Sort::REVERSE)?;
    walk.push(head)?;
    walk.hide(base)?;
    let mut tree = jj.get_tree_oid_for_commit(base)?;
    for oid in walk {
        let commit = repo.find_commit(oid?)?;
        if commit.parent_count() != 1 {
            continue;
        }
        match jj.merge_trees(commit.parent(0)?.tree_id(), tree, commit.tree_id())? {
            Some(merged) => tree = merged,
            None => return Ok(None),
        }
    }
    Ok(Some(tree))
}

/// Resolve the effective cherry-pick state from CLI flags and the "Cherry Pick:"
/// marker on the commit description.
///
/// When `--cherry-pick` is passed the marker is added (if absent); when
/// `--no-cherry-pick` is passed the marker is removed (if present). Returns
/// `(effective_cherry_pick, marker_was_changed)`.
fn resolve_cherry_pick(
    cherry_pick: bool,
    no_cherry_pick: bool,
    message: &mut crate::message::MessageSectionsMap,
) -> (bool, bool) {
    let stored = crate::message::cherry_pick_marked(message);

    if no_cherry_pick {
        if stored {
            message.remove(&MessageSection::CherryPick);
            return (false, true);
        }
        (false, false)
    } else if cherry_pick {
        if !stored {
            message.insert(MessageSection::CherryPick, "true".to_string());
            return (true, true);
        }
        (true, false)
    } else {
        (stored, false)
    }
}

/// Work out what `diff_impl` needs for one change, failing for a missing
/// title or description, a cherry-pick that conflicts, or an unknown
/// reviewer. `diff` does this for every change before it changes anything on
/// GitHub.
async fn resolve_change(
    opts: &PushOptions,
    jj: &crate::jj::Jujutsu,
    config: &crate::config::Config,
    commit: &crate::jj::PreparedCommit,
    master_base_oid: Oid,
) -> Result<Resolved> {
    let label = format!(
        "{} {}",
        commit.short_id,
        commit
            .message
            .get(&MessageSection::Title)
            .map(String::as_str)
            .unwrap_or("(no title)")
    );

    // Each change of a Pull Request of several changes can become a commit on
    // its branch, with the change's description as the message.
    let undescribed: Vec<&str> = commit
        .lower
        .iter()
        .filter(|m| {
            m.message
                .get(&MessageSection::Title)
                .is_none_or(|title| title.trim().is_empty())
        })
        .map(|m| m.short_id.as_str())
        .collect();
    if !undescribed.is_empty() {
        return Err(Error::new(format!(
            "Not submitting: {} {} no description, and each change of a Pull \
             Request of several changes becomes a commit on its branch. Describe \
             them with `jj describe`.",
            undescribed.join(", "),
            if undescribed.len() == 1 {
                "has"
            } else {
                "have"
            },
        )));
    }

    let mut message = commit.message.clone();
    let (cherry_pick, mut message_changed) =
        resolve_cherry_pick(opts.cherry_pick, opts.no_cherry_pick, &mut message);

    // Without --cherry-pick, these are the trees of the change and its
    // parent. A change directly on master gets the same trees when it is
    // cherry-picked onto its parent.
    let (head_tree, base_tree) = if !cherry_pick || commit.parent_oid == master_base_oid {
        (
            jj.get_tree_oid_for_commit(commit.oid)?,
            jj.get_tree_oid_for_commit(commit.parent_oid)?,
        )
    } else {
        let Some(cherry_pick_tree) =
            jj.cherry_pick_tree(commit.parent_oid, commit.oid, master_base_oid)?
        else {
            return Err(Error::new(format!(
                "Not submitting: {label} cannot be cherry-picked on {}.",
                config.master_ref.branch_name(),
            )));
        };
        (
            cherry_pick_tree,
            jj.get_tree_oid_for_commit(master_base_oid)?,
        )
    };

    if commit.pull_request_number.is_none() || opts.update_message {
        validate_commit_message(&message).context(format!("Not submitting {label}"))?;
    }

    // A new Pull Request requests the reviewers in the message, which is
    // rewritten to show each user's name.
    let mut reviewers = PullRequestRequestReviewers::default();
    if commit.pull_request_number.is_none()
        && let Some(names) = message.get(&MessageSection::Reviewers)
    {
        let unknown = |kind: &str, reviewer: &str| {
            Error::new(format!(
                "Not submitting: the Reviewers field of {label} contains unknown \
                 {kind} '{reviewer}'",
            ))
        };
        let mut checked = Vec::new();
        for reviewer in parse_name_list(names) {
            // Teams are indicated with a leading #
            if let Some(slug) = reviewer.strip_prefix('#') {
                let Ok(team) = GitHub::get_github_team((&config.owner).into(), slug.into()).await
                else {
                    return Err(unknown("team", &reviewer));
                };
                reviewers.team_reviewers.push(team.slug.to_string());
                checked.push(reviewer);
            } else {
                let Ok(user) = GitHub::get_github_user(reviewer.clone()).await else {
                    return Err(unknown("user", &reviewer));
                };
                reviewers.reviewers.push(user.login);
                checked.push(match user.name {
                    Some(name) => format!("{reviewer} ({})", remove_all_parens(&name)),
                    None => reviewer,
                });
            }
        }
        message.insert(MessageSection::Reviewers, checked.join(", "));
        message_changed = true;
    }

    Ok(Resolved {
        message,
        message_changed,
        cherry_pick,
        head_tree,
        base_tree,
        reviewers,
    })
}

pub async fn diff(
    opts: DiffOptions,
    jj: &crate::jj::Jujutsu,
    gh: &mut crate::github::GitHub,
    config: &crate::config::Config,
) -> Result<()> {
    // With --tips, the range runs from the base up to the topmost tip, as
    // with --all.
    let tips = opts
        .tips
        .as_deref()
        .map(|revset| resolve_tips(jj, revset))
        .transpose()?;
    let topmost_tip = tips.as_ref().map(|(topmost, _)| topmost.to_string());

    // Determine revision and whether to use range mode
    let (use_range_mode, base_rev, target_rev, is_inclusive) =
        crate::revision_utils::parse_revision_and_range(
            topmost_tip.as_deref().or(opts.revision.as_deref()),
            opts.all || tips.is_some(),
            opts.base.as_deref(),
        )?;

    // Get commits to process
    let mut prepared_commits = if use_range_mode {
        // Get range of commits from base to target
        jj.get_prepared_commits_from_to(config, &base_rev, &target_rev, is_inclusive)?
    } else {
        // Just get the single specified revision
        vec![jj.get_prepared_commit_for_revision(config, &target_rev)?]
    };
    if opts.one_pr && !use_range_mode {
        return Err(Error::new(
            "--one-pr needs a range of changes, like `-r A::C` or `--all`.".to_string(),
        ));
    }
    let tips = match tips {
        Some((_, tips)) => tips,
        // One Pull Request is the whole selection, cut after its top change.
        None if opts.one_pr => prepared_commits.last().map(|c| c.oid).into_iter().collect(),
        None => Vec::new(),
    };
    prepared_commits = crate::jj::join_at_tips(prepared_commits, &tips)?;
    if (opts.title.is_some() || opts.body.is_some()) && prepared_commits.len() > 1 {
        return Err(Error::new(format!(
            "Not submitting: --title and --body are for one Pull Request, and this \
             run has {}. Run `jj spr diff` once per Pull Request.",
            prepared_commits.len()
        )));
    }

    // The commit on master that the changes are based on. The bottom change
    // is not directly on it when its parent is outside the selection.
    let master_base_oid = if let Some(first_commit) = prepared_commits.first() {
        jj.get_master_base_for_commit(config, first_commit.oid)?
    } else {
        output("👋", "No commits found - nothing to do. Good bye!")?;
        return Ok(());
    };

    // A conflicted change would be pushed as jj's conflict files.
    let selected = prepared_commits
        .iter()
        .flat_map(|c| c.member_oids())
        .map(|oid| oid.to_string())
        .collect::<Vec<_>>()
        .join(" | ");
    let conflicted = jj.conflicted_changes(&selected)?;
    if !conflicted.is_empty() {
        return Err(Error::new(format!(
            "Not submitting: these changes have conflicts:\n{conflicted}\nResolve them \
             (for example with `jj resolve`) and run `jj spr diff` again.",
        )));
    }

    refuse_unreadable_pull_request_lines(jj, config, &prepared_commits)?;

    let pull_requests = gh
        .get_pull_requests(prepared_commits.iter().map(|pc| pc.pull_request_number))
        .await?;

    // A merged Pull Request cannot be updated, and pushing the rest of the
    // stack on top of its local change would retarget them at its branch and
    // dissolve their GitHub stack. `jj spr sync` abandons it instead.
    let merged: Vec<u64> = pull_requests
        .iter()
        .flatten()
        .filter(|pr| pr.state == PullRequestState::Merged)
        .map(|pr| pr.number)
        .collect();
    if !merged.is_empty() {
        let has = if merged.len() == 1 { "has" } else { "have" };
        return Err(Error::new(format!(
            "{} {has} already merged. Run `jj spr sync` to abandon the merged \
             change(s) and rebase the rest of the stack onto {}, then run \
             `jj spr diff` again.",
            crate::stacks::format_pr_list(&merged),
            config.trunk_revset(),
        )));
    }

    let closed: Vec<u64> = pull_requests
        .iter()
        .flatten()
        .filter(|pr| pr.state == PullRequestState::Closed)
        .map(|pr| pr.number)
        .collect();
    if !closed.is_empty() {
        return Err(Error::new(format!(
            "Not submitting: {} closed. If you want to open a new one, remove \
             the 'Pull Request' section from the commit message.",
            if closed.len() == 1 {
                format!("Pull Request #{} is", closed[0])
            } else {
                format!(
                    "Pull Requests {} are",
                    crate::stacks::format_pr_list(&closed)
                )
            }
        )));
    }

    // --title and --body are for a run with one Pull Request.
    let overrides = vec![
        Overrides {
            title: opts.title.clone(),
            body: opts.body.clone(),
            branch: None,
        };
        prepared_commits.len()
    ];
    let mut record = Descriptions {
        jj,
        config,
        dry_run: opts.dry_run,
    };
    publish(
        &mut opts.push_options(),
        jj,
        gh,
        config,
        &mut prepared_commits,
        &overrides,
        pull_requests,
        master_base_oid,
        &mut record,
    )
    .await
}

/// Publish `commits`, a line of planned Pull Requests (bottom first), with
/// `pull_requests` the existing ones among them: check everything that can
/// stop the run, then push and create or update each one, bottom first, and
/// register them as a GitHub stack. `overrides` has an entry for each one.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn publish(
    opts: &mut PushOptions,
    jj: &crate::jj::Jujutsu,
    gh: &mut crate::github::GitHub,
    config: &crate::config::Config,
    prepared_commits: &mut [crate::jj::PreparedCommit],
    overrides: &[Overrides],
    mut pull_requests: Vec<Option<PullRequest>>,
    master_base_oid: Oid,
    record: &mut dyn Record,
) -> Result<()> {
    let mut result = Ok(());

    let mut resolved = Vec::with_capacity(prepared_commits.len());
    for commit in prepared_commits.iter() {
        resolved.push(resolve_change(opts, jj, config, commit, master_base_oid).await?);
    }

    // Everything below compares against the Pull Requests' heads as fetched;
    // stop if one could not be fetched as GitHub reports it.
    for pr in pull_requests.iter().flatten() {
        if let Some(github_head) = pr.github_head_oid
            && pr.state == PullRequestState::Open
            && github_head != pr.head_oid
        {
            return Err(Error::new(format!(
                "Could not fetch the current head of Pull Request #{} (GitHub reports {}, \
                 the local copy of {} is {}). Run `jj git fetch` and try again.",
                pr.number,
                github_head,
                pr.head.branch_name(),
                pr.head_oid
            )));
        }
    }

    // Pushing replaces a Pull Request's content with the local change's, so
    // stop before changing anything if that would drop someone else's work.
    if !opts.discard_remote_changes {
        let mut changed_remotely = Vec::new();
        for ((commit, pull_request), resolved) in
            zip(zip(prepared_commits.iter(), &pull_requests), &resolved)
        {
            if let Some(pr) = pull_request
                && pr.state == PullRequestState::Open
                && !resolved.cherry_pick
                && jj.pr_has_remote_changes(config, commit, pr)?
            {
                changed_remotely.push(pr.number);
            }
        }
        if let Some(first) = changed_remotely.first() {
            let (has, s, them) = if changed_remotely.len() == 1 {
                ("has", "", "it")
            } else {
                ("have", "s", "them")
            };
            return Err(Error::new(format!(
                "{} {has} commits that are not in the local change{s} (someone else \
                 pushed to {them}). Run `jj spr patch {first}` to update your local \
                 changes from GitHub, or pass --discard-remote-changes to replace \
                 them with your version.",
                crate::stacks::format_pr_list(&changed_remotely),
            )));
        }
    }

    let mut taken_branches = taken_branch_names(jj, config, &pull_requests).await?;

    // A bottom change that is not on master targets its parent's Pull
    // Request, which has to exist and be open.
    let outside_parent =
        if prepared_commits[0].parent_oid != master_base_oid && !resolved[0].cherry_pick {
            let bottom_parent = jj.prepare_commit(config, prepared_commits[0].parent_oid)?;
            Some(lookup_parent_pull_request(gh, &bottom_parent).await?)
        } else {
            None
        };

    // GitHub refuses to change the base of a Pull Request that is part of a
    // stack. Work out up front which stacks this run is
    // about to invalidate, and dissolve them before changing anything; they
    // are rebuilt once every Pull Request is published. A stack with members
    // this run does not publish cannot be rebuilt, so that stops the run.
    if let Some(stacks) = gh.list_stacks().await? {
        let planned = plan_native_bases(
            config,
            prepared_commits,
            &pull_requests,
            &resolved,
            outside_parent.as_ref(),
            master_base_oid,
        );
        let to_dissolve = crate::stacks::plan_unstacks(&stacks, &planned).map_err(Error::new)?;
        // Each updated Pull Request needs a message for its new commit. Ask
        // now, once for all of them, so that aborting at the prompt leaves
        // the stacks in place.
        if !to_dissolve.is_empty() && opts.message.is_none() && !opts.dry_run {
            let input = tokio::task::spawn_blocking(|| {
                dialoguer::Input::<String>::new()
                    .with_prompt("Message for the updated Pull Requests (leave empty to abort)")
                    .allow_empty(true)
                    .interact_text()
            })
            .await??;
            if input.is_empty() {
                return Err(Error::new("Aborted as per user request".to_string()));
            }
            opts.message = Some(input);
        }
        for stack in to_dissolve {
            if opts.dry_run {
                output(
                    "🧱",
                    &format!("Would dissolve GitHub stack #{stack} to rebuild it"),
                )?;
            } else {
                output(
                    "🧱",
                    &format!("Dissolving GitHub stack #{stack} to rebuild it"),
                )?;
                gh.unstack(stack).await?;
            }
        }
    }

    // GitHub marks a Pull Request merged once its head is in its base branch.
    // After a reorder, a Pull Request's new head can contain the old head of
    // a dependent Pull Request that is updated later: moving C below A and B
    // puts B's old head (already in C's branch) into A's branch while B still
    // targets A. Point such a dependent Pull Request at master until its own
    // update sets its new base.
    if !opts.dry_run {
        for i in 0..pull_requests.len() {
            let Some(branch) = pull_requests[i].as_ref().map(|pr| pr.head.clone()) else {
                continue;
            };
            for j in i + 1..pull_requests.len() {
                let Some(pr) = pull_requests[j]
                    .as_ref()
                    .filter(|pr| pr.base.branch_name() == branch.branch_name())
                else {
                    continue;
                };
                let mut contained = false;
                for below in pull_requests[..i].iter().flatten() {
                    contained |= is_ancestor(jj, pr.head_oid, below.head_oid)?;
                }
                if !contained {
                    continue;
                }
                let number = pr.number;
                output(
                    "↪️ ",
                    &format!(
                        "Pointing #{number} at {} until its update, so that GitHub \
                         does not mark it merged",
                        config.master_ref.branch_name()
                    ),
                )?;
                gh.update_pull_request(
                    number,
                    PullRequestUpdate {
                        base: Some(config.master_ref.branch_name().to_string()),
                        ..Default::default()
                    },
                )
                .await?;
                if let Some(pr) = pull_requests[j].as_mut() {
                    pr.base = config.master_ref.clone();
                }
            }
        }
    }

    let mut message_on_prompt = "".to_string();
    // Published Pull Requests in local parent order, for registering stacks.
    let mut published_links: Vec<Published> = outside_parent
        .iter()
        .map(|parent| Published {
            number: parent.number,
            head: parent.branch.clone(),
            head_oid: parent.head_oid,
            base: None,
        })
        .collect();
    let mut parent = outside_parent;

    for (index, ((prepared_commit, pull_request), resolved)) in
        zip(zip(prepared_commits.iter_mut(), pull_requests), resolved).enumerate()
    {
        if result.is_err() {
            break;
        }

        if !opts.dry_run {
            write_commit_title(prepared_commit)?;
        }

        // A change that is not directly on master is stacked on its parent's
        // Pull Request: the one published just before it, or for the bottom
        // change, `outside_parent`.
        let stacked_on_parent =
            prepared_commit.parent_oid != master_base_oid && !resolved.cherry_pick;

        // The further implementation of the diff command is in a separate function.
        // This makes it easier to run the code to update the local commit message
        // with all the changes that the implementation makes at the end, even if
        // the implementation encounters an error or exits early.
        let outcome = diff_impl(
            opts,
            &mut message_on_prompt,
            jj,
            gh,
            config,
            prepared_commit,
            resolved,
            &overrides[index],
            &mut |branch: &str| record.pushing(index, branch),
            master_base_oid,
            pull_request,
            &mut taken_branches,
            if stacked_on_parent {
                parent.as_ref()
            } else {
                None
            },
        )
        .await;

        match outcome {
            Ok(published) => {
                parent = Some(ParentPullRequest {
                    number: published.number,
                    branch: published.head.clone(),
                    head_oid: published.head_oid,
                });
                add_error(
                    &mut result,
                    record.published(index, prepared_commit, published.number),
                );
                published_links.push(published);
            }
            Err(error) => result = Err(error),
        }
    }

    if result.is_ok() {
        add_error(
            &mut result,
            register_native_stacks(opts, gh, config, &published_links).await,
        );
    }

    add_error(&mut result, record.finish(prepared_commits));

    if opts.dry_run {
        let actions: Vec<_> = prepared_commits
            .iter()
            .enumerate()
            .filter(|(_, c)| c.dry_run_action.is_some())
            .collect();

        output(
            "\n📋",
            &format!(
                "Dry run complete. Would process {} change(s):\n",
                actions.len()
            ),
        )?;

        for (idx, pc) in &actions {
            let title = overrides[*idx].title.as_deref().unwrap_or_else(|| {
                pc.message
                    .get(&crate::message::MessageSection::Title)
                    .map(|t| &t[..])
                    .unwrap_or("")
            });
            let pos = idx + 1;

            let (action_label, base, head, reviewers_list) = match &pc.dry_run_action {
                Some(crate::jj::DryRunAction::Create {
                    base,
                    head,
                    draft,
                    reviewers,
                    ..
                }) => {
                    let label = if *draft {
                        "CREATE (draft)".to_string()
                    } else {
                        "CREATE".to_string()
                    };
                    (label, base.as_str(), head.as_str(), reviewers.clone())
                }
                Some(crate::jj::DryRunAction::Update {
                    pr_number,
                    base,
                    head,
                    ..
                }) => {
                    let label = format!("UPDATE PR #{pr_number}");
                    (label, base.as_str(), head.as_str(), vec![])
                }
                None => continue,
            };

            output(
                &format!("  #{pos}"),
                &format!("{action_label}  {}  \"{title}\"", pc.short_id),
            )?;
            output("     ", &format!("head: {head}"))?;
            output("     ", &format!("base: {base}"))?;
            if !reviewers_list.is_empty() {
                output(
                    "     ",
                    &format!("reviewers: {}", reviewers_list.join(", ")),
                )?;
            }
            output("", "")?;
        }
    }

    result
}

/// The branch names a new Pull Request's branch must not take, from one
/// request to GitHub: the branches with jj-spr's prefix on GitHub and in
/// this repository, and those of the existing Pull Requests. Empty when
/// every Pull Request exists.
async fn taken_branch_names(
    jj: &crate::jj::Jujutsu,
    config: &crate::config::Config,
    pull_requests: &[Option<PullRequest>],
) -> Result<HashSet<String>> {
    if pull_requests.iter().all(Option::is_some) {
        return Ok(HashSet::new());
    }
    let pattern = format!("refs/heads/{}*", config.branch_prefix);
    let on_github = jj.remote_heads(&config.remote_name, &[pattern]).await?;

    let remote_prefix = format!("refs/remotes/{}/", config.remote_name);
    let mut taken: HashSet<String> = jj
        .get_all_ref_names()?
        .iter()
        .filter_map(|name| name.strip_prefix(&remote_prefix))
        .map(String::from)
        .collect();
    taken.extend(on_github.into_keys());
    taken.extend(
        pull_requests
            .iter()
            .flatten()
            .map(|pr| pr.head.branch_name().to_string()),
    );
    Ok(taken)
}

/// Predict each selected Pull Request's base branch after this run, for
/// deciding which GitHub stacks survive it.
fn plan_native_bases(
    config: &crate::config::Config,
    commits: &[crate::jj::PreparedCommit],
    pull_requests: &[Option<PullRequest>],
    resolved: &[Resolved],
    outside_parent: Option<&ParentPullRequest>,
    master_base_oid: Oid,
) -> Vec<crate::stacks::PlannedPullRequest> {
    commits
        .iter()
        .enumerate()
        .map(|(i, commit)| {
            let pull_request = pull_requests[i].as_ref();
            let expected_base = if commit.parent_oid == master_base_oid || resolved[i].cherry_pick {
                Some(config.master_ref.branch_name().to_string())
            } else if i == 0 {
                outside_parent.map(|parent| parent.branch.branch_name().to_string())
            } else {
                pull_requests[i - 1]
                    .as_ref()
                    .map(|parent| parent.head.branch_name().to_string())
            };
            crate::stacks::PlannedPullRequest {
                number: pull_request.map(|pr| pr.number),
                current_base: pull_request.map(|pr| pr.base.branch_name().to_string()),
                expected_base,
            }
        })
        .collect()
}

/// Refuse when a change's `Pull Request:` field is there but cannot be read,
/// which `jj squash` of two submitted changes causes by keeping both lines.
/// Treating it as no Pull Request would open a new one and orphan the old.
fn refuse_unreadable_pull_request_lines(
    jj: &crate::jj::Jujutsu,
    config: &crate::config::Config,
    commits: &[crate::jj::PreparedCommit],
) -> Result<()> {
    let unreadable: Vec<&crate::jj::PreparedCommit> = commits
        .iter()
        .flat_map(|c| c.lower.iter().chain([c]))
        .filter(|c| c.pull_request_number.is_none())
        .filter(|c| {
            c.message
                .get(&MessageSection::PullRequest)
                .is_some_and(|field| !field.trim().is_empty())
        })
        .collect();
    let Some(first) = unreadable.first() else {
        return Ok(());
    };
    let field = first.message[&MessageSection::PullRequest].trim();
    let lines: Vec<&str> = field
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect();
    let links: Vec<&str> = lines
        .iter()
        .copied()
        .filter(|l| config.parse_pull_request_field(l).is_some())
        .collect();
    let owned: Vec<crate::jj::PreparedCommit> = vec![(*first).clone()];
    let id = jj
        .change_ids_for(&owned)?
        .get(&first.oid)
        .map(|id| crate::jj::short(id).to_string())
        .unwrap_or_else(|| first.short_id.clone());
    let (what, shown) = if links.len() > 1 {
        (
            "links more than one Pull Request (as after `jj squash` of two \
             submitted changes)",
            links,
        )
    } else {
        ("has a `Pull Request:` line that jj-spr cannot read", lines)
    };
    Err(Error::new(format!(
        "Not submitting: {id} {what}:\n{}\nKeep one `Pull Request:` line (with \
         `jj describe`), and close the other Pull Request on GitHub if it is no \
         longer needed.",
        shown
            .iter()
            .map(|l| format!("  {l}"))
            .collect::<Vec<_>>()
            .join("\n")
    )))
}

/// Find the Pull Request of the bottom change's parent, which its Pull
/// Request targets when the parent is not part of this run.
async fn lookup_parent_pull_request(
    gh: &crate::github::GitHub,
    parent_commit: &crate::jj::PreparedCommit,
) -> Result<ParentPullRequest> {
    let number = parent_commit.pull_request_number.ok_or_else(|| {
        Error::new(formatdoc!(
            "The parent change {} has no Pull Request yet. A change's Pull \
             Request targets its parent's Pull Request, so submit the whole \
             stack with `jj spr diff --all`.",
            parent_commit.short_id
        ))
    })?;
    let pull_request = gh.get_pull_request(number).await?;
    match pull_request.state {
        PullRequestState::Open => {}
        PullRequestState::Merged => {
            return Err(Error::new(formatdoc!(
                "The parent change's Pull Request #{number} has merged. Run `jj \
                 spr sync` to abandon the merged change and rebase this one, then \
                 run `jj spr diff` again.",
            )));
        }
        PullRequestState::Closed => {
            return Err(Error::new(formatdoc!(
                "The parent change's Pull Request #{number} is closed. Rebase this \
                 change before submitting it.",
            )));
        }
    }
    Ok(ParentPullRequest {
        number: Some(number),
        branch: pull_request.head,
        head_oid: pull_request.head_oid,
    })
}

/// Register the published chains of Pull Requests as native GitHub stacks.
/// The Pull Requests are already correct at this point, so problems with the
/// stack itself are reported as warnings.
async fn register_native_stacks(
    opts: &PushOptions,
    gh: &crate::github::GitHub,
    config: &crate::config::Config,
    published: &[Published],
) -> Result<()> {
    use crate::stacks::{
        PublishedLink, StackAction, chain_segments, format_pr_list, plan_stack_updates,
        segment_numbers,
    };

    let links: Vec<PublishedLink> = published
        .iter()
        .map(|p| PublishedLink {
            number: p.number,
            base: p.base.clone(),
            head: p.head.branch_name().to_string(),
        })
        .collect();
    let segments = chain_segments(&links);
    if segments.is_empty() {
        return Ok(());
    }

    if opts.dry_run {
        for segment in segments {
            let labels: Vec<String> = segment
                .iter()
                .map(|l| l.number.map_or("new PR".to_string(), |n| format!("#{n}")))
                .collect();
            output(
                "🧱",
                &format!("Would register GitHub stack: {}", labels.join(" ← ")),
            )?;
        }
        return Ok(());
    }
    let segments = segment_numbers(&segments);

    let Some(stacks) = gh.list_stacks().await? else {
        output(
            "ℹ️ ",
            &format!(
                "Stacked pull requests are not enabled for {}/{}; the Pull Requests \
                 target each other but are not registered as a GitHub stack.",
                config.owner, config.repo
            ),
        )?;
        return Ok(());
    };

    for action in plan_stack_updates(&stacks, &segments) {
        match action {
            StackAction::Create(pull_requests) => match gh.create_stack(&pull_requests).await {
                Ok(stack) => output(
                    "🧱",
                    &format!(
                        "Created GitHub stack #{}: {}",
                        stack.number,
                        format_pr_list(&pull_requests)
                    ),
                )?,
                Err(error) => {
                    output("⚠️", "Creating the GitHub stack failed")?;
                    for message in error.messages() {
                        output("  ", message)?;
                    }
                }
            },
            StackAction::Add {
                stack,
                pull_requests,
            } => match gh.add_to_stack(stack, &pull_requests).await {
                Ok(_) => output(
                    "🧱",
                    &format!(
                        "Added {} to GitHub stack #{stack}",
                        format_pr_list(&pull_requests)
                    ),
                )?,
                Err(error) => {
                    output("⚠️", &format!("Adding to GitHub stack #{stack} failed"))?;
                    for message in error.messages() {
                        output("  ", message)?;
                    }
                }
            },
            StackAction::Rebuild {
                stack,
                pull_requests,
            } => {
                output(
                    "🧱",
                    &format!(
                        "Dissolving GitHub stack #{stack}, which ends with a closed Pull \
                         Request, to rebuild it"
                    ),
                )?;
                let rebuilt = async {
                    gh.unstack(stack).await?;
                    gh.create_stack(&pull_requests).await
                };
                match rebuilt.await {
                    Ok(new) => output(
                        "🧱",
                        &format!(
                            "Created GitHub stack #{}: {}",
                            new.number,
                            format_pr_list(&pull_requests)
                        ),
                    )?,
                    Err(error) => {
                        output("⚠️", "Rebuilding the GitHub stack failed")?;
                        for message in error.messages() {
                            output("  ", message)?;
                        }
                    }
                }
            }
            StackAction::Warn(message) => output("⚠️", &message)?,
        }
    }

    Ok(())
}

/// What a run changes in an existing Pull Request's title and body: the ones
/// given in `overrides` (with --title and --body), or with --update-message,
/// the local description's.
fn title_and_body_updates(
    opts: &PushOptions,
    overrides: &Overrides,
    pull_request: &PullRequest,
    message: &crate::message::MessageSectionsMap,
) -> PullRequestUpdate {
    let mut updates = PullRequestUpdate::default();
    if opts.update_message {
        updates.update_message(pull_request, message);
    }
    if let Some(title) = overrides
        .title
        .as_ref()
        .filter(|t| **t != pull_request.title)
    {
        updates.title = Some(title.clone());
    }
    if let Some(body) = overrides
        .body
        .as_ref()
        .filter(|b| pull_request.body.as_ref() != Some(b))
    {
        updates.body = Some(body.clone());
    }
    updates
}

#[allow(clippy::too_many_arguments)]
async fn diff_impl(
    opts: &PushOptions,
    message_on_prompt: &mut String,
    jj: &crate::jj::Jujutsu,
    gh: &mut crate::github::GitHub,
    config: &crate::config::Config,
    local_commit: &mut crate::jj::PreparedCommit,
    resolved: Resolved,
    overrides: &Overrides,
    pushing_new: &mut dyn FnMut(&str) -> Result<()>,
    master_base_oid: Oid,
    pull_request: Option<PullRequest>,
    taken_branches: &mut HashSet<String>,
    parent: Option<&ParentPullRequest>,
) -> Result<Published> {
    let Resolved {
        message: resolved_message,
        message_changed,
        cherry_pick: effective_cherry_pick,
        head_tree: new_head_tree,
        base_tree: new_base_tree,
        reviewers: requested_reviewers,
    } = resolved;
    local_commit.message = resolved_message;
    local_commit.message_changed |= message_changed;
    let message = &mut local_commit.message;

    // Check if the local commit is based directly on the master branch.
    let directly_based_on_master = local_commit.parent_oid == master_base_oid;

    if let Some(number) = local_commit.pull_request_number {
        output(
            "#️⃣ ",
            &format!(
                "Pull Request #{}: {}",
                number,
                config.pull_request_url(number)
            ),
        )?;
    }

    // A Pull Request of several changes matches when it has the title and
    // message of any of them, since changes added on top later keep the
    // title it was opened with. A run that sets the title or body has no
    // need for the warning.
    if let Some(ref pull_request) = pull_request
        && !opts.update_message
        && overrides.title.is_none()
        && overrides.body.is_none()
    {
        let matches = |message: &crate::message::MessageSectionsMap| {
            let mut updates: PullRequestUpdate = Default::default();
            updates.update_message(pull_request, message);
            updates.is_empty()
        };
        if !matches(message) && !local_commit.lower.iter().any(|m| matches(&m.message)) {
            let advice = if local_commit.lower.is_empty() {
                "The Pull Request's title/message differ from the local commit's \
                 message.\nUse `spr diff --update-message` to overwrite the title and \
                 message on GitHub with the local message, or `spr amend` to go the \
                 other way (rewrite the local commit message with what is on GitHub)."
                    .to_string()
            } else {
                // `spr amend` would overwrite the top change's description.
                format!(
                    "The Pull Request's title/message differ from the messages of its \
                     changes.\nUse `spr diff --update-message` to overwrite the title \
                     and message on GitHub with those of {}, the top of its {} changes.",
                    local_commit.short_id,
                    local_commit.lower.len() + 1
                )
            };
            output("⚠️", &advice)?;
        }
    }

    // Get the name of the existing Pull Request branch, or constuct one if
    // there is none yet.

    let title = message
        .get(&MessageSection::Title)
        .map(|t| &t[..])
        .unwrap_or("");
    let pr_title = overrides.title.as_deref().unwrap_or(title);

    let pull_request_branch = match (&pull_request, &overrides.branch) {
        (Some(pr), _) => pr.head.clone(),
        (None, Some(branch)) => config.new_github_branch(branch),
        (None, None) => {
            let name = config.get_new_branch_name(taken_branches, pr_title);
            taken_branches.insert(name.clone());
            config.new_github_branch(&name)
        }
    };

    // Get the tree ids of the current head of the Pull Request, as well as the
    // base, and the commit id of the master commit this PR is currently based
    // on.
    // If there is no pre-existing Pull Request, we fill in the equivalent
    // values.
    let (pr_head_oid, pr_head_tree, pr_base_tree, pr_master_base) = if let Some(pr) = &pull_request
    {
        let pr_head_tree = jj.get_tree_oid_for_commit(pr.head_oid)?;

        let current_master_oid = jj.resolve_reference(config.master_ref.local())?;
        // Use git for merge base calculation since jj doesn't expose this directly
        let pr_base_oid = jj.git_repo.merge_base(pr.head_oid, pr.base_oid)?;
        let pr_base_tree = jj.get_tree_oid_for_commit(pr_base_oid)?;

        let pr_master_base = jj.git_repo.merge_base(pr.head_oid, current_master_oid)?;

        (pr.head_oid, pr_head_tree, pr_base_tree, pr_master_base)
    } else {
        let master_base_tree = jj.get_tree_oid_for_commit(master_base_oid)?;
        (
            master_base_oid,
            master_base_tree,
            master_base_tree,
            master_base_oid,
        )
    };
    let needs_merging_master = pr_master_base != master_base_oid;

    // The Pull Request targets its parent's Pull Request branch, or master
    // when there is no parent Pull Request.
    let base_branch = parent.map_or(&config.master_ref, |parent| &parent.branch);

    // An existing Pull Request also needs an update when it does not target
    // (and contain) its parent's current Pull Request branch, or targets
    // anything but master (such as a base branch that older versions of
    // jj-spr made for it) when it has no parent Pull Request.
    let base_is_expected = match (&pull_request, parent) {
        (Some(pr), Some(parent)) => {
            pr.base.branch_name() == parent.branch.branch_name()
                && is_ancestor(jj, parent.head_oid, pr.head_oid)?
        }
        (Some(pr), None) => pr.base.is_master_branch(),
        (None, _) => true,
    };

    // At this point we can check if we can exit early because no update to the
    // existing Pull Request is necessary
    if let Some(ref pull_request) = pull_request {
        // So there is an existing Pull Request...
        if !needs_merging_master
            && base_is_expected
            && pr_head_tree == new_head_tree
            && pr_base_tree == new_base_tree
        {
            // ...and it does not need a rebase, and the trees of both Pull
            // Request branch and base are all the right ones.
            output("✅", "No update necessary")?;

            let pull_request_updates =
                title_and_body_updates(opts, overrides, pull_request, message);
            if !pull_request_updates.is_empty() {
                if opts.dry_run {
                    output(
                        "  ",
                        &format!("Would update PR #{} title/body", pull_request.number),
                    )?;
                } else {
                    gh.update_pull_request(pull_request.number, pull_request_updates)
                        .await?;
                    output("✍", "Updated commit message on GitHub")?;
                }
            }

            return Ok(Published {
                number: Some(pull_request.number),
                head: pull_request.head.clone(),
                head_oid: pull_request.head_oid,
                base: Some(pull_request.base.branch_name().to_string()),
            });
        }
    }

    // The commit to merge into the new Pull Request commit, if any, to bring
    // in changes to the local change's parent.
    let pr_base_parent = if let Some(parent) = parent {
        // The Pull Request's new commit merges in the head of its parent's
        // Pull Request branch. That keeps GitHub's diff (against the merge
        // base) down to exactly this change.
        if !opts.dry_run && jj.get_tree_oid_for_commit(parent.head_oid)? != new_base_tree {
            return Err(Error::new(formatdoc!(
                "The Pull Request for the parent change{} does not contain the \
                 parent change as it is locally (was it submitted with \
                 --cherry-pick, or not updated yet?). Run `jj spr diff --all` \
                 to update the whole stack.",
                parent
                    .number
                    .map(|n| format!(" (#{n})"))
                    .unwrap_or_default()
            )));
        }
        let merge_in_parent =
            pull_request.is_none() || !is_ancestor(jj, parent.head_oid, pr_head_oid)?;
        merge_in_parent.then_some(parent.head_oid)
    } else if pr_base_tree == new_base_tree && !needs_merging_master && base_is_expected {
        // Nothing to merge in: the local change's parent is unchanged.
        None
    } else {
        // The Pull Request targets master, so it merges in the master commit
        // the local change is based on.
        Some(master_base_oid)
    };

    // Construct the new commit for the Pull Request branch. First parent is the
    // current head commit of the Pull Request (we set this to the master base
    // commit earlier if the Pull Request does not yet exist)
    let mut pr_commit_parents = if pull_request.is_none() && parent.is_some() {
        // A new Pull Request with a parent Pull Request starts on top of its
        // parent's Pull Request branch.
        vec![]
    } else {
        vec![pr_head_oid]
    };

    // If we prepared a commit earlier that needs merging into the Pull Request
    // branch, then that commit is a parent of the new Pull Request commit.
    if let Some(oid) = pr_base_parent {
        // ...unless if that's the same commit as the one we added to
        // pr_commit_parents first.
        if pr_commit_parents.first() != Some(&oid) {
            pr_commit_parents.push(oid);
        }
    }

    // In a GitHub stack, GitHub rebases the Pull Requests left after a
    // stacked merge: it replays the non-merge commits of each branch that are
    // not on its old base, and drops merge commits. A Pull Request left with
    // nothing to replay is reset to its base and closed. So an update has to
    // leave the change in non-merge commits. When the old head is in the new
    // head, as for a Pull Request that moved above the one that had its
    // commits, the update goes straight onto the new head. When the new head
    // is in the old one, it goes onto the old head. A merge would add
    // nothing in either case. Otherwise the merge commit gets the tree that
    // replaying the branch's existing non-merge commits onto the new head
    // would produce. When that tree differs from the local change's, an
    // ordinary commit on top brings it to the local change's tree. When the
    // replay conflicts, the update stays one merge commit.
    if pr_commit_parents.len() == 2 && !opts.dry_run {
        let (old_head, new_head) = (pr_commit_parents[0], pr_commit_parents[1]);
        if is_ancestor(jj, old_head, new_head)? {
            pr_commit_parents = vec![new_head];
        } else if is_ancestor(jj, new_head, old_head)? {
            pr_commit_parents = vec![old_head];
        } else if let Some(replayed) = replayed_tree(jj, old_head, new_head)?
            && replayed != new_head_tree
        {
            let merge_commit = jj.create_derived_commit(
                local_commit.oid,
                "[spr] changes introduced through rebase",
                replayed,
                &pr_commit_parents[..],
            )?;
            pr_commit_parents = vec![merge_commit];
        }
    }

    // A Pull Request of several changes gets a commit for each change when
    // its branch has the tree of the base or of one of its changes: one for
    // each change above that one. Otherwise it gets one commit with the top
    // change's tree.
    let mut steps = Vec::new();
    if !local_commit.lower.is_empty()
        && pr_commit_parents.len() == 1
        && (!effective_cherry_pick || directly_based_on_master)
    {
        let mut trees = vec![new_base_tree];
        for member in &local_commit.lower {
            trees.push(jj.get_tree_oid_for_commit(member.oid)?);
        }
        trees.push(new_head_tree);
        // The tree the steps go on.
        let current = jj.get_tree_oid_for_commit(pr_commit_parents[0])?;
        if let Some(done) = trees.iter().rposition(|tree| *tree == current) {
            let members = local_commit.lower.iter().map(|m| (m.oid, &m.message));
            let members = members.chain([(local_commit.oid, &*message)]);
            steps = zip(members, &trees[1..])
                .skip(done)
                .map(|((oid, message), tree)| {
                    let text =
                        build_message(message, &[MessageSection::Title, MessageSection::Summary]);
                    (oid, text, *tree)
                })
                .collect();
        }
    }

    let mut github_commit_message = opts.message.clone();
    if pull_request.is_some()
        && github_commit_message.is_none()
        && steps.is_empty()
        && !opts.dry_run
    {
        let input = {
            let message_on_prompt = message_on_prompt.clone();

            tokio::task::spawn_blocking(move || {
                dialoguer::Input::<String>::new()
                    .with_prompt("Message (leave empty to abort)")
                    .with_initial_text(message_on_prompt)
                    .allow_empty(true)
                    .interact_text()
            })
            .await??
        };

        if input.is_empty() {
            return Err(Error::new("Aborted as per user request".to_string()));
        }

        *message_on_prompt = input.clone();
        github_commit_message = Some(input);
    }

    let mut published_number = pull_request.as_ref().map(|pr| pr.number);

    // Create the new commit
    let pr_commit = if opts.dry_run {
        // Use a placeholder OID — this won't be pushed
        pr_head_oid
    } else if !steps.is_empty() {
        let mut parents = pr_commit_parents;
        for (oid, text, tree) in steps {
            parents = vec![jj.create_derived_commit(oid, &text, tree, &parents)?];
        }
        parents[0]
    } else {
        jj.create_derived_commit(
            local_commit.oid,
            github_commit_message
                .as_ref()
                .map(|s| &s[..])
                .unwrap_or_else(|| title),
            new_head_tree,
            &pr_commit_parents[..],
        )?
    };

    if opts.dry_run {
        let base_branch_name = base_branch.branch_name();
        let head_branch_name = pull_request_branch.branch_name();
        let is_stacked = !base_branch.is_master_branch();

        local_commit.dry_run_action = if let Some(ref pr) = pull_request {
            Some(crate::jj::DryRunAction::Update {
                pr_number: pr.number,
                base: base_branch_name.to_string(),
                head: head_branch_name.to_string(),
                is_stacked,
            })
        } else {
            let all_reviewers: Vec<String> = requested_reviewers
                .reviewers
                .iter()
                .chain(requested_reviewers.team_reviewers.iter())
                .cloned()
                .collect();
            Some(crate::jj::DryRunAction::Create {
                base: base_branch_name.to_string(),
                head: head_branch_name.to_string(),
                is_stacked,
                draft: opts.draft,
                reviewers: all_reviewers,
            })
        };
    } else {
        let mut cmd = jj.git_command();
        cmd.arg("push")
            .arg("--atomic")
            .arg("--no-verify")
            .arg("--")
            .arg(&config.remote_name)
            .arg(format!("{}:{}", pr_commit, pull_request_branch.on_github()));

        if let Some(pull_request) = pull_request {
            // We are updating an existing Pull Request

            if needs_merging_master {
                output(
                    "⚾",
                    &format!(
                        "Commit was rebased - updating Pull Request #{}",
                        pull_request.number
                    ),
                )?;
            } else {
                output(
                    "🔁",
                    &format!(
                        "Commit was changed - updating Pull Request #{}",
                        pull_request.number
                    ),
                )?;
            }

            // Things we want to update in the Pull Request on GitHub
            let mut pull_request_updates =
                title_and_body_updates(opts, overrides, &pull_request, message);

            // Push the new commit onto the Pull Request branch.
            run_command(&mut cmd)
                .await
                .reword("git push failed".to_string())?;

            // Retarget the Pull Request if its parent Pull Request changed,
            // or it has none any more (it landed, or the parent change was
            // abandoned locally).
            if pull_request.base.branch_name() != base_branch.branch_name() {
                pull_request_updates.base = Some(base_branch.branch_name().to_string());
            }

            if !pull_request_updates.is_empty() {
                let changes_base = pull_request_updates.base.is_some();
                let update = gh
                    .update_pull_request(pull_request.number, pull_request_updates)
                    .await;
                if changes_base {
                    update.context(
                        "GitHub does not allow changing the base of a Pull Request \
                         in a stack. Run `jj spr diff --all` from the top of the \
                         stack so jj-spr can rebuild it."
                            .to_string(),
                    )?;
                } else {
                    update?;
                }
            }
        } else {
            // We are creating a new Pull Request.

            // Push the Pull Request branch. Its base, the parent's Pull
            // Request branch or master, is on GitHub already.
            pushing_new(pull_request_branch.branch_name())?;
            run_command(&mut cmd)
                .await
                .reword("git push failed".to_string())?;

            // Then call GitHub to create the Pull Request.
            let pull_request_number = gh
                .create_pull_request(
                    pr_title,
                    overrides
                        .body
                        .clone()
                        .unwrap_or_else(|| build_github_body(message)),
                    base_branch.branch_name().to_string(),
                    pull_request_branch.branch_name().to_string(),
                    opts.draft,
                )
                .await?;

            published_number = Some(pull_request_number);
            let pull_request_url = config.pull_request_url(pull_request_number);

            output(
                "✨",
                &format!(
                    "Created new Pull Request #{}: {}",
                    pull_request_number, pull_request_url,
                ),
            )?;

            let result = gh
                .request_reviewers(pull_request_number, requested_reviewers)
                .await;
            match result {
                Ok(()) => (),
                Err(error) => {
                    output("⚠️", "Requesting reviewers failed")?;
                    for message in error.messages() {
                        output("  ", message)?;
                    }
                }
            }
        }
    }

    Ok(Published {
        number: published_number,
        head: pull_request_branch,
        head_oid: pr_commit,
        base: Some(base_branch.branch_name().to_string()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    fn create_test_config() -> crate::config::Config {
        crate::config::Config::new(
            "test_owner".into(),
            "test_repo".into(),
            "origin".into(),
            "main".into(),
            "spr/test/".into(),
            false,
        )
    }

    #[allow(dead_code)]
    fn create_test_git_repo() -> (TempDir, git2::Repository) {
        let temp_dir = TempDir::new().expect("Failed to create temp directory");
        let repo = git2::Repository::init(temp_dir.path()).expect("Failed to init git repo");

        // Create initial commit
        let signature = git2::Signature::now("Test User", "test@example.com")
            .expect("Failed to create signature");
        let tree_id = {
            let mut index = repo.index().expect("Failed to get index");
            index.write_tree().expect("Failed to write tree")
        };
        let tree = repo.find_tree(tree_id).expect("Failed to find tree");

        repo.commit(
            Some("HEAD"),
            &signature,
            &signature,
            "Initial commit",
            &tree,
            &[],
        )
        .expect("Failed to create initial commit");

        drop(tree); // Drop the tree reference before moving repo
        (temp_dir, repo)
    }

    #[allow(dead_code)]
    fn create_test_commit(repo: &git2::Repository, message: &str, content: &str) -> git2::Oid {
        let signature = git2::Signature::now("Test User", "test@example.com")
            .expect("Failed to create signature");

        // Write content to a test file
        let repo_path = repo.workdir().expect("Failed to get workdir");
        let file_path = repo_path.join("test.txt");
        fs::write(&file_path, content).expect("Failed to write test file");

        // Add file to index
        let mut index = repo.index().expect("Failed to get index");
        index
            .add_path(std::path::Path::new("test.txt"))
            .expect("Failed to add file to index");
        index.write().expect("Failed to write index");

        let tree_id = index.write_tree().expect("Failed to write tree");
        let tree = repo.find_tree(tree_id).expect("Failed to find tree");

        // Get HEAD commit as parent
        let parent_commit = repo
            .head()
            .expect("Failed to get HEAD")
            .peel_to_commit()
            .expect("Failed to peel to commit");

        repo.commit(
            Some("HEAD"),
            &signature,
            &signature,
            message,
            &tree,
            &[&parent_commit],
        )
        .expect("Failed to create commit")
    }

    #[test]
    fn test_diff_options_default_values() {
        let opts = DiffOptions {
            all: false,
            update_message: false,
            title: None,
            body: None,
            draft: false,
            message: None,
            cherry_pick: false,
            no_cherry_pick: false,
            base: None,
            revision: None,
            dry_run: false,
            discard_remote_changes: false,
            one_pr: false,
            tips: None,
        };

        assert!(!opts.all);
        assert!(!opts.update_message);
        assert!(!opts.draft);
        assert!(!opts.cherry_pick);
        assert!(opts.message.is_none());
        assert!(opts.base.is_none());
    }

    #[test]
    fn test_diff_options_with_base() {
        let opts = DiffOptions {
            all: true,
            update_message: false,
            title: None,
            body: None,
            draft: false,
            message: None,
            cherry_pick: false,
            no_cherry_pick: false,
            base: Some("main".to_string()),
            revision: None,
            dry_run: false,
            discard_remote_changes: false,
            one_pr: false,
            tips: None,
        };

        assert_eq!(opts.base, Some("main".to_string()));
        assert!(opts.all);
    }

    #[test]
    fn test_jujutsu_integration() {
        // Test configuration for jj-spr
        let config = create_test_config();
        assert_eq!(config.owner, "test_owner");
        assert_eq!(config.remote_name, "origin");
    }

    #[test]
    fn test_base_option_parsing() {
        // Test that the base option can be parsed correctly
        let opts_with_base = DiffOptions {
            all: true,
            update_message: false,
            title: None,
            body: None,
            draft: false,
            message: None,
            cherry_pick: false,
            no_cherry_pick: false,
            base: Some("main".to_string()),
            revision: None,
            dry_run: false,
            discard_remote_changes: false,
            one_pr: false,
            tips: None,
        };

        assert_eq!(opts_with_base.base.as_deref(), Some("main"));
        assert!(opts_with_base.all);

        let opts_with_trunk = DiffOptions {
            all: true,
            update_message: false,
            title: None,
            body: None,
            draft: false,
            message: None,
            cherry_pick: false,
            no_cherry_pick: false,
            base: Some("trunk()".to_string()),
            revision: None,
            dry_run: false,
            discard_remote_changes: false,
            one_pr: false,
            tips: None,
        };

        assert_eq!(opts_with_trunk.base.as_deref(), Some("trunk()"));
    }

    #[test]
    fn test_all_flag_behavior() {
        let opts_with_all = DiffOptions {
            all: true,
            update_message: false,
            title: None,
            body: None,
            draft: false,
            message: None,
            cherry_pick: false,
            no_cherry_pick: false,
            base: Some("trunk()".to_string()),
            revision: None,
            dry_run: false,
            discard_remote_changes: false,
            one_pr: false,
            tips: None,
        };

        // When --all is specified, it should work with base revisions
        assert!(opts_with_all.all);
        assert!(opts_with_all.base.is_some());
    }

    #[test]
    fn test_diff_options_combinations() {
        // Test various valid combinations of options
        let opts = DiffOptions {
            all: true,
            update_message: true,
            title: None,
            body: None,
            draft: true,
            message: Some("Update message".to_string()),
            cherry_pick: false,
            no_cherry_pick: false,
            base: Some("trunk()".to_string()),
            revision: None,
            dry_run: false,
            discard_remote_changes: false,
            one_pr: false,
            tips: None,
        };

        assert!(opts.all);
        assert!(opts.update_message);
        assert!(opts.draft);
        assert_eq!(opts.message.as_deref(), Some("Update message"));
        assert!(!opts.cherry_pick);
        assert_eq!(opts.base.as_deref(), Some("trunk()"));
    }

    #[test]
    fn test_diff_options_dry_run_flag() {
        let opts = DiffOptions {
            all: false,
            update_message: false,
            title: None,
            body: None,
            draft: false,
            message: None,
            cherry_pick: false,
            no_cherry_pick: false,
            base: None,
            revision: None,
            dry_run: true,
            discard_remote_changes: false,
            one_pr: false,
            tips: None,
        };

        assert!(opts.dry_run);
        assert!(!opts.all);
    }

    // -------------------------------------------------------------------------
    // resolve_cherry_pick unit tests

    fn make_map_with_cherry_pick(value: &str) -> crate::message::MessageSectionsMap {
        [(
            crate::message::MessageSection::CherryPick,
            value.to_string(),
        )]
        .into()
    }

    #[test]
    fn test_resolve_cherry_pick_default_no_marker() {
        let mut map = crate::message::MessageSectionsMap::new();
        let (effective, changed) = resolve_cherry_pick(false, false, &mut map);
        assert!(!effective);
        assert!(!changed);
        assert!(!map.contains_key(&crate::message::MessageSection::CherryPick));
    }

    #[test]
    fn test_resolve_cherry_pick_default_with_marker() {
        let mut map = make_map_with_cherry_pick("true");
        let (effective, changed) = resolve_cherry_pick(false, false, &mut map);
        assert!(effective);
        assert!(!changed);
    }

    #[test]
    fn test_resolve_cherry_pick_marker_case_insensitive() {
        let mut map = make_map_with_cherry_pick("TRUE");
        let (effective, _) = resolve_cherry_pick(false, false, &mut map);
        assert!(effective);
    }

    #[test]
    fn test_resolve_cherry_pick_marker_other_value() {
        for value in &["false", "yes", "1", ""] {
            let mut map = make_map_with_cherry_pick(value);
            let (effective, _) = resolve_cherry_pick(false, false, &mut map);
            assert!(!effective, "Expected false for marker value {:?}", value);
        }
    }

    #[test]
    fn test_resolve_cherry_pick_flag_inserts_marker() {
        let mut map = crate::message::MessageSectionsMap::new();
        let (effective, changed) = resolve_cherry_pick(true, false, &mut map);
        assert!(effective);
        assert!(changed);
        assert_eq!(
            map.get(&crate::message::MessageSection::CherryPick)
                .map(String::as_str),
            Some("true")
        );
    }

    #[test]
    fn test_resolve_cherry_pick_flag_idempotent() {
        let mut map = make_map_with_cherry_pick("true");
        let (effective, changed) = resolve_cherry_pick(true, false, &mut map);
        assert!(effective);
        assert!(!changed);
    }

    #[test]
    fn test_resolve_cherry_pick_no_flag_removes_marker() {
        let mut map = make_map_with_cherry_pick("true");
        let (effective, changed) = resolve_cherry_pick(false, true, &mut map);
        assert!(!effective);
        assert!(changed);
        assert!(!map.contains_key(&crate::message::MessageSection::CherryPick));
    }

    #[test]
    fn test_resolve_cherry_pick_no_flag_when_absent() {
        let mut map = crate::message::MessageSectionsMap::new();
        let (effective, changed) = resolve_cherry_pick(false, true, &mut map);
        assert!(!effective);
        assert!(!changed);
    }

    #[test]
    fn test_diff_options_parse_no_cherry_pick() {
        use clap::Parser;
        let opts = DiffOptions::try_parse_from(["test", "--no-cherry-pick"]).unwrap();
        assert!(opts.no_cherry_pick);
        assert!(!opts.cherry_pick);
    }

    #[test]
    fn test_diff_options_cherry_pick_no_cherry_pick_conflict() {
        use clap::Parser;
        let result = DiffOptions::try_parse_from(["test", "--cherry-pick", "--no-cherry-pick"]);
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert_eq!(err.kind(), clap::error::ErrorKind::ArgumentConflict);
    }

    #[test]
    fn test_diff_options_title_and_body() {
        use clap::Parser;
        use clap::error::ErrorKind;
        let kind = |args: &[&str]| {
            DiffOptions::try_parse_from(["test"].iter().chain(args))
                .map(|_| ())
                .map_err(|e| e.kind())
        };
        for flag in ["--title", "--body"] {
            assert_eq!(
                kind(&[flag, "x", "--update-message"]),
                Err(ErrorKind::ArgumentConflict)
            );
        }
        for blank in ["", "  "] {
            assert_eq!(kind(&["--title", blank]), Err(ErrorKind::ValueValidation));
        }
        assert_eq!(kind(&["--body", ""]), Ok(()));

        let opts = DiffOptions::try_parse_from(["test", "--title", " Title "]).unwrap();
        assert_eq!(opts.title.as_deref(), Some("Title"));
    }

    // Integration tests would require more complex setup with actual Git repositories
    // and proper mocking of GitHub API calls. The tests above focus on:
    // 1. Option parsing and validation
    // 2. Data structure correctness
    // 3. Basic logic flow verification
    //
    // For full integration testing, consider:
    // - Mocking GitHub API responses
    // - Creating test repositories with specific commit structures
    // - Testing the interaction between revision specification and commit preparation
}
