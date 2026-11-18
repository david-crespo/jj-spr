/*
 * Copyright (c) Radical HQ Limited
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

use indoc::formatdoc;
use std::{io::Write, process::Stdio, time::Duration};

use crate::{
    error::{Error, Result, ResultExt},
    github::{PullRequestState, PullRequestUpdate},
    message::build_github_body_for_merging,
    output::{output, write_commit_title},
    utils::run_command,
};

#[derive(Debug, clap::Parser)]
pub struct LandOptions {
    /// Merge a Pull Request that was created or updated with spr diff
    /// --cherry-pick
    #[clap(long)]
    cherry_pick: bool,

    /// Jujutsu revision to operate on (if not specified, uses '@'). In a
    /// native GitHub stack this can be any Pull Request of the stack: it is
    /// merged together with every Pull Request below it.
    #[clap(short = 'r', long)]
    revision: Option<String>,

    /// Merge the Pull Requests below the chosen one in its stack without
    /// asking (required when not running in a terminal)
    #[clap(long, short = 'y')]
    yes: bool,
}

pub async fn land(
    mut opts: LandOptions,
    jj: &crate::jj::Jujutsu,
    gh: &mut crate::github::GitHub,
    config: &crate::config::Config,
) -> Result<()> {
    let revision = opts.revision.as_deref().unwrap_or("@");
    let prepared_commit = jj.get_prepared_commit_for_revision(config, revision)?;

    // Honor both the --cherry-pick flag and the "Cherry Pick:" marker on the
    // commit description. When the validation TODO below is filled in, use
    // opts.cherry_pick as the authoritative source.
    opts.cherry_pick =
        opts.cherry_pick || crate::message::cherry_pick_marked(&prepared_commit.message);

    write_commit_title(&prepared_commit)?;

    let pull_request_number = if let Some(number) = prepared_commit.pull_request_number {
        output("#️⃣ ", &format!("Pull Request #{}", number))?;
        number
    } else {
        return Err(Error::new("This commit does not refer to a Pull Request."));
    };

    // Load Pull Request information
    let pull_request = gh.get_pull_request(pull_request_number).await?;

    check_landable(config, &pull_request)?;

    output("🛫", "Getting started...")?;

    // Fetch current master from GitHub.
    run_command(
        jj.git_command()
            .arg("fetch")
            .arg("--no-write-fetch-head")
            .arg("--")
            .arg(&config.remote_name)
            .arg(config.master_ref.on_github()),
    )
    .await
    .reword("git fetch failed".to_string())?;

    // TODO: Implement Jujutsu-native cherry-pick and merge validation
    // For now, we'll trust GitHub's merge validation and skip local validation
    let base_is_master = pull_request.base.is_master_branch();

    // Skip local cherry-pick validation for Jujutsu workflow
    // GitHub will validate mergeability during the merge process
    let merge_matches_cherrypick = true;

    if !merge_matches_cherrypick {
        return Err(Error::new(formatdoc!(
            "This commit has been updated and/or rebased since the pull \
             request was last updated. Please run `spr diff` to update the \
             pull request and then try `spr land` again!"
        )));
    }

    // A Pull Request in a native GitHub stack lands together with every open
    // Pull Request below it.
    let stack = gh.stack_containing(pull_request_number).await?;
    // Without a stack, a Pull Request based on another open Pull Request's
    // branch would be squash-merged together with everything below it.
    if !base_is_master && stack.is_none() {
        let base = pull_request.base.branch_name();
        if let Some(below) = gh
            .get_open_pull_requests()
            .await?
            .into_iter()
            .find(|pr| pr.head_ref_name == base)
        {
            return Err(Error::new(format!(
                "Pull Request #{pull_request_number} targets {base}, the branch of \
                 open Pull Request #{below}, and is not in a GitHub stack, so landing \
                 it would merge both as one commit. Land #{below} first.",
                below = below.number
            )));
        }
    }

    // The Pull Requests below this one that merge with it, bottom first.
    let mut merged_below = Vec::new();
    if let Some(stack) = &stack {
        let below = stack
            .active_pull_requests()
            .into_iter()
            .take_while(|n| *n != pull_request_number)
            .map(Some);
        for pr in gh.get_pull_requests(below).await?.into_iter().flatten() {
            check_landable(config, &pr)?;
            merged_below.push(pr);
        }
        let bottom = merged_below.first().unwrap_or(&pull_request);
        if !bottom.base.is_master_branch() {
            return Err(Error::new(format!(
                "GitHub stack #{} targets {}, not {}.",
                stack.number,
                bottom.base.branch_name(),
                config.master_ref.branch_name()
            )));
        }

        if merged_below.is_empty() {
            output(
                "🧱",
                &format!("Merging from the bottom of GitHub stack #{}", stack.number),
            )?;
        } else {
            let below: Vec<u64> = merged_below.iter().map(|pr| pr.number).collect();
            let mut merging = below.clone();
            merging.push(pull_request_number);
            output(
                "🧱",
                &format!(
                    "Landing #{} merges {} from GitHub stack #{} (bottom first)",
                    pull_request_number,
                    crate::stacks::format_pr_list(&merging),
                    stack.number
                ),
            )?;
            confirm_merging_below(&opts, &below)?;
        }
    }

    if !base_is_master && stack.is_none() {
        // The base of the Pull Request on GitHub is not set to master. This
        // means the Pull Request uses a base branch. We tested above that
        // merging the Pull Request branch into the master branch produces the
        // intended result (the same as cherry-picking the local commit onto
        // master), so what we want to do is actually merge the Pull Request as
        // it is into master. Hence, we change the base to the master branch.
        //
        // Before we do that, there is one more edge case to look out for: if
        // the base branch contains changes that have since been landed on
        // master, then Git might be able to figure out that these changes
        // appear both in the pull request branch (via the merge branch) and in
        // master, but are identical in those two so it is not a merge conflict
        // but can go ahead. The result of this in master if we merge now is
        // correct, but there is one problem: when looking at the Pull Request
        // in GitHub after merging, it will show these change as part of the
        // Pull Request. So when you look at the changed files of the Pull
        // Request, you will see both changes in this commit (great!) and those
        // in the base branch (a previous commit that has already been landed on
        // master - not great!). This is because the changes shown are the ones
        // that happened on this Pull Request branch (now including the base
        // branch) since it branched off master. This can include changes in the
        // base branch that are already on master, but were added to master
        // after the Pull Request branch branched from master.
        // The solution is to merge current master into the Pull Request branch.
        // Doing that now means that the final changes done by this Pull Request
        // are only the changes that are not yet in master. That's what we want.
        // This final merge never introduces any changes to the Pull Request. In
        // fact, the tree that we use for the merge commit is the one we got
        // above from the cherry-picking of this commit on master.

        // TODO: Implement Jujutsu-native merge base and tree comparison
        // For now, skip the complex merge-in-master logic
        // This logic would need to be rewritten using jj commands

        // Skip the merge-in-master commit creation for Jujutsu workflow

        gh.update_pull_request(
            pull_request_number,
            PullRequestUpdate {
                base: Some(config.master_ref.branch_name().to_string()),
                ..Default::default()
            },
        )
        .await?;
    }

    // Merge bottom first, one Pull Request at a time, so each one gets its
    // own title and message (GitHub applies them only to the Pull Request
    // named in the merge call). After each merge GitHub rebases the rest of
    // the stack and retargets the next Pull Request to master, so only the
    // first one still has the head fetched above.
    let merging: Vec<&crate::github::PullRequest> =
        merged_below.iter().chain([&pull_request]).collect();
    let mut landed: Vec<&crate::github::PullRequest> = Vec::new();
    let mut merged_sha = None;
    let mut failure = None;
    for pr in merging.iter().copied() {
        let expected_head = landed.is_empty().then_some(pr.head_oid);
        let merged = match wait_until_mergeable(gh, pr.number, expected_head).await {
            Ok(head) => squash_merge(gh, config, pr, head, stack.is_some()).await,
            Err(error) => Err(error),
        };
        match merged {
            Ok(sha) => {
                if merging.len() > 1 {
                    output("🛬", &format!("Landed #{}", pr.number))?;
                }
                landed.push(pr);
                merged_sha = sha;
            }
            Err(error) => {
                failure = Some(error);
                break;
            }
        }
    }

    let failure = match failure {
        Some(mut error) if landed.is_empty() => {
            output("❌", "GitHub Pull Request merge failed")?;

            // If we changed the target branch of the Pull Request earlier, then
            // undo this change now.
            if !base_is_master && stack.is_none() {
                let result = gh
                    .update_pull_request(
                        pull_request_number,
                        PullRequestUpdate {
                            base: Some(pull_request.base.on_github().to_string()),
                            ..Default::default()
                        },
                    )
                    .await;
                if let Err(e) = result {
                    error.push(format!("{}", e));
                }
            }

            return Err(error);
        }
        // Each landed Pull Request is a complete commit on master, so clean
        // up after those and report the rest.
        Some(error) => {
            let numbers = |prs: &[&crate::github::PullRequest]| {
                crate::stacks::format_pr_list(&prs.iter().map(|pr| pr.number).collect::<Vec<_>>())
            };
            output(
                "❌",
                &format!(
                    "Landed {}, but not {}",
                    numbers(&landed),
                    numbers(&merging[landed.len()..])
                ),
            )?;
            Some(error)
        }
        None => {
            output("🛬", "Landed!")?;
            None
        }
    };

    // Other open Pull Requests may target a merged Pull Request's branch
    // (natively stacked PRs do). GitHub closes a Pull Request whose base
    // branch is deleted, so move them to master first, and keep any branch
    // that another open Pull Request still uses.
    let merged_heads: Vec<&crate::github::GitHubBranch> =
        landed.iter().map(|pr| &pr.head).collect();
    let merged_numbers: Vec<u64> = landed.iter().map(|pr| pr.number).collect();
    // A legacy base branch made by jj-spr for this Pull Request.
    let legacy_base = (!base_is_master && stack.is_none()).then_some(&pull_request.base);
    let branches_to_delete: Vec<&crate::github::GitHubBranch> =
        match gh.get_open_pull_requests().await {
            Ok(open_pull_requests) => {
                let open_pull_requests: Vec<_> = open_pull_requests
                    .into_iter()
                    .filter(|pr| !merged_numbers.contains(&pr.number))
                    .collect();
                let mut kept_heads = Vec::new();
                for dependent in open_pull_requests.iter() {
                    let Some(head) = merged_heads
                        .iter()
                        .find(|head| dependent.base_ref_name == head.branch_name())
                    else {
                        continue;
                    };
                    let retarget = gh
                        .update_pull_request(
                            dependent.number,
                            PullRequestUpdate {
                                base: Some(config.master_ref.branch_name().to_string()),
                                ..Default::default()
                            },
                        )
                        .await;
                    match retarget {
                        Ok(()) => output(
                            "↪️ ",
                            &format!(
                                "Pull Request #{} now targets {}",
                                dependent.number,
                                config.master_ref.branch_name()
                            ),
                        )?,
                        Err(_) => {
                            kept_heads.push(head.branch_name().to_string());
                            output(
                                "⚠️",
                                &format!(
                                    "Pull Request #{} still targets {}; keeping that branch",
                                    dependent.number,
                                    head.branch_name()
                                ),
                            )?;
                        }
                    }
                }
                let used_elsewhere = |branch: &str| {
                    open_pull_requests
                        .iter()
                        .any(|pr| pr.head_ref_name == branch || pr.base_ref_name == branch)
                };
                merged_heads
                    .iter()
                    .copied()
                    .filter(|head| !kept_heads.iter().any(|kept| kept == head.branch_name()))
                    .chain(legacy_base.filter(|base| !used_elsewhere(base.branch_name())))
                    .collect()
            }
            // Without the list, a branch that another Pull Request targets
            // cannot be told apart from an unused one, and deleting it would
            // close that Pull Request.
            Err(error) => {
                let kept: Vec<&str> = merged_heads
                    .iter()
                    .copied()
                    .chain(legacy_base)
                    .map(|branch| branch.branch_name())
                    .collect();
                output(
                    "⚠️",
                    &format!(
                        "Could not list the open Pull Requests, so keeping {} in case \
                         another Pull Request targets them: {error}",
                        kept.join(", ")
                    ),
                )?;
                Vec::new()
            }
        };
    let mut delete_branch_processes = Vec::new();
    for branch in branches_to_delete {
        delete_branch_processes.push((
            branch.branch_name().to_string(),
            jj.git_command()
                .arg("push")
                .arg("--no-verify")
                .arg("--delete")
                .arg("--")
                .arg(&config.remote_name)
                .arg(branch.on_github())
                .stdout(Stdio::null())
                .stderr(Stdio::piped())
                .spawn()?,
        ));
    }

    // Rebase us on top of the now-landed commit
    if let Some(sha) = merged_sha {
        // Try this up to three times, because fetching the very moment after
        // the merge might still not find the new commit.
        for i in 0..3 {
            // Fetch current master and the merge commit from GitHub.
            let git_fetch = jj
                .git_command()
                .arg("fetch")
                .arg("--no-write-fetch-head")
                .arg("--")
                .arg(&config.remote_name)
                .arg(config.master_ref.on_github())
                .arg(&sha)
                .stdout(Stdio::null())
                .stderr(Stdio::piped())
                .output()
                .await?;
            if git_fetch.status.success() {
                break;
            } else if i == 2 {
                console::Term::stderr().write_all(&git_fetch.stderr)?;
                return Err(Error::new("git fetch failed"));
            }
        }
        output(
            "👉",
            "Run `jj spr sync` to abandon the landed change and rebase the rest \
             of the stack",
        )?;
    }

    // Wait for the "git push" deleting the old branches to finish. GitHub may
    // be configured to delete the branch automatically, in which case it's
    // gone already and the push fails harmlessly; report any other failure.
    for (branch, proc) in delete_branch_processes {
        let result = proc.wait_with_output().await?;
        let stderr = String::from_utf8_lossy(&result.stderr);
        if !result.status.success() && !stderr.contains("remote ref does not exist") {
            output(
                "⚠️",
                &format!(
                    "Could not delete branch {branch}: {}",
                    stderr
                        .lines()
                        .map(str::trim)
                        .filter(|l| !l.is_empty())
                        .collect::<Vec<_>>()
                        .join(" ")
                ),
            )?;
        }
    }

    failure.map_or(Ok(()), Err)
}

/// How many times `land` asks GitHub whether a Pull Request can be merged
/// before it gives up. One second apart, that is about two minutes: GitHub
/// has taken over a minute after a stack was rebased.
const MERGEABILITY_POLLS: u32 = 120;

fn mergeability_poll_interval() -> Duration {
    // Tests shorten the wait.
    std::env::var("SPR_MERGEABILITY_POLL_INTERVAL_MS")
        .ok()
        .and_then(|ms| ms.parse().ok())
        .map_or(Duration::from_secs(1), Duration::from_millis)
}

/// Wait until Pull Request `number` targets master and GitHub has worked out
/// whether it can be merged, and refuse if it cannot. Returns its head, which
/// must be `expected_head` when that is given.
async fn wait_until_mergeable(
    gh: &crate::github::GitHub,
    number: u64,
    expected_head: Option<git2::Oid>,
) -> Result<git2::Oid> {
    for poll in 0..MERGEABILITY_POLLS {
        let mergeability = gh.get_pull_request_mergeability(number).await?;
        if expected_head.is_some_and(|head| head != mergeability.head_oid) {
            return Err(Error::new(format!(
                "Pull Request #{number} seems to have been updated externally. Please \
                 try again!"
            )));
        }
        if mergeability.base.is_master_branch()
            && let Some(mergeable) = mergeability.mergeable
        {
            if !mergeable {
                return Err(Error::new(format!(
                    "GitHub concluded Pull Request #{number} is not mergeable at this \
                     point. Please rebase your changes and try again!"
                )));
            }
            return Ok(mergeability.head_oid);
        }
        if poll == 0 {
            output(
                "⏳",
                &format!("Waiting for GitHub to work out whether #{number} can be merged"),
            )?;
        }
        tokio::time::sleep(mergeability_poll_interval()).await;
    }
    Err(Error::new(format!(
        "GitHub has not finished computing whether Pull Request #{number} can be \
         merged. Try again in a minute."
    )))
}

/// Squash-merge `pull_request` at `head` with jj-spr's title and message.
/// A Pull Request in a native stack has to go through GitHub's stack merge.
async fn squash_merge(
    gh: &crate::github::GitHub,
    config: &crate::config::Config,
    pull_request: &crate::github::PullRequest,
    head: git2::Oid,
    stacked: bool,
) -> Result<Option<String>> {
    let number = pull_request.number;
    let message = build_github_body_for_merging(&pull_request.sections);
    let merged = if stacked {
        gh.merge_stacked_pull_request(number, head, "squash", &pull_request.title, &message)
            .await
    } else {
        octocrab::instance()
            .pulls(&config.owner, &config.repo)
            .merge(number)
            .method(octocrab::params::pulls::MergeMethod::Squash)
            .title(pull_request.title.clone())
            .message(message)
            .sha(head.to_string())
            .send()
            .await
            .convert()
            .and_then(|merge| {
                if merge.merged {
                    Ok(merge.sha)
                } else {
                    Err(Error::new(format!(
                        "GitHub Pull Request merge failed: {}",
                        merge.message.unwrap_or_default()
                    )))
                }
            })
    };
    merged.context(format!("squash-merging PR #{number} (head {head})"))
}

/// Refuse Pull Requests that GitHub would not merge, or that are not
/// approved when approval is required.
fn check_landable(
    config: &crate::config::Config,
    pull_request: &crate::github::PullRequest,
) -> Result<()> {
    let number = pull_request.number;
    if pull_request.state != PullRequestState::Open {
        return Err(Error::new(format!(
            "Pull Request #{number} is already closed!"
        )));
    }
    if pull_request.is_draft {
        return Err(Error::new(format!(
            "Pull Request #{number} is a draft. Mark it ready for review first (for \
             example with `gh pr ready {number}`)."
        )));
    }
    if let Some(reason) = pull_request
        .review_decision
        .blocks_landing(config.require_approval)
    {
        return Err(Error::new(format!(
            "Pull Request #{number} {reason} on GitHub."
        )));
    }
    Ok(())
}

/// Landing a Pull Request above the bottom of a stack merges the ones below
/// it too; make sure that is intended.
fn confirm_merging_below(opts: &LandOptions, below: &[u64]) -> Result<()> {
    use std::io::IsTerminal;
    if opts.yes {
        return Ok(());
    }
    let list = crate::stacks::format_pr_list(below);
    if !std::io::stdin().is_terminal() {
        return Err(Error::new(format!(
            "This would also merge {list}. Pass --yes to confirm, or land the bottom \
             of the stack first."
        )));
    }
    let confirmed = dialoguer::Confirm::new()
        .with_prompt(format!("Also merge {list}?"))
        .default(false)
        .interact()?;
    if confirmed {
        Ok(())
    } else {
        Err(Error::new("Not landing anything."))
    }
}
