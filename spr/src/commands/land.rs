/*
 * Copyright (c) Radical HQ Limited
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

use indoc::formatdoc;
use std::{
    io::Write,
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};

use crate::{
    error::{Error, Result, ResultExt},
    github::{PullRequestState, PullRequestUpdate},
    message::build_github_body_for_merging,
    output::{output, write_commit_title},
    stack_file::StackFile,
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

    /// A stack file whose Pull Requests to land
    #[clap(value_name = "STACK_FILE", conflicts_with = "revision")]
    file: Option<PathBuf>,

    /// With a stack file, land only the entries up to and including the one
    /// with this tip
    #[clap(long, value_name = "TIP", requires = "file")]
    upto: Option<String>,
}

pub async fn land(
    mut opts: LandOptions,
    jj: &crate::jj::Jujutsu,
    gh: &mut crate::github::GitHub,
    config: &crate::config::Config,
) -> Result<()> {
    if let Some(path) = &opts.file {
        return land_stack_file(path, opts.upto.as_deref(), jj, gh, config).await;
    }
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
    land_pull_request(pull_request_number, opts.yes, None, jj, gh, config).await
}

/// Land the Pull Requests of a stack file's entries, bottom first, up to the
/// one with tip `upto`. In a GitHub stack, that is one landing of the top
/// one, which merges the ones below it; the stack has to have exactly those
/// below it. Without one, each lands on its own.
async fn land_stack_file(
    path: &Path,
    upto: Option<&str>,
    jj: &crate::jj::Jujutsu,
    gh: &mut crate::github::GitHub,
    config: &crate::config::Config,
) -> Result<()> {
    let file = StackFile::read(path)?;
    let name = path.display();
    let numbers = file
        .active(upto)?
        .into_iter()
        .map(|i| {
            let entry = &file.prs[i];
            entry.pr.ok_or_else(|| {
                Error::new(format!(
                    "'{}' has no Pull Request yet. Run `jj spr submit {name}` first.",
                    entry.tip
                ))
            })
        })
        .collect::<Result<Vec<u64>>>()?;
    let Some((&top, below)) = numbers.split_last() else {
        output("👋", "Every entry has merged. Nothing to land.")?;
        return Ok(());
    };
    let merged: Vec<u64> = gh
        .get_pull_requests(numbers.iter().map(|&n| Some(n)))
        .await?
        .into_iter()
        .flatten()
        .filter(|pr| pr.state == PullRequestState::Merged)
        .map(|pr| pr.number)
        .collect();
    if !merged.is_empty() {
        return Err(Error::new(format!(
            "Not landing: {} merged already. Run {} first.",
            format_or_nothing(&merged),
            spr_command("sync", Some(path))
        )));
    }
    match gh.stack_containing(top).await? {
        Some(stack) => {
            let stacked: Vec<u64> = stack
                .active_pull_requests()
                .into_iter()
                .take_while(|n| *n != top)
                .collect();
            if stacked != below {
                return Err(Error::new(format!(
                    "Not landing: below #{top}, GitHub stack #{} has {}, but {name} has {}. \
                     Run `jj spr submit {name}` to bring GitHub up to date.",
                    stack.number,
                    format_or_nothing(&stacked),
                    format_or_nothing(below),
                )));
            }
            output("#️⃣ ", &format!("Pull Request #{top}"))?;
            land_pull_request(top, true, Some(path), jj, gh, config).await
        }
        None => {
            for &number in &numbers {
                output("#️⃣ ", &format!("Pull Request #{number}"))?;
                land_pull_request(number, true, Some(path), jj, gh, config).await?;
            }
            Ok(())
        }
    }
}

/// `jj spr <command>` as the user runs it, with the stack file being landed
/// if there is one, quoted for output.
fn spr_command(command: &str, file: Option<&Path>) -> String {
    match file {
        Some(path) => format!("`jj spr {command} {}`", path.display()),
        None => format!("`jj spr {command}`"),
    }
}

fn pr_numbers(prs: &[&crate::github::PullRequest]) -> String {
    crate::stacks::format_pr_list(&prs.iter().map(|pr| pr.number).collect::<Vec<_>>())
}

fn format_or_nothing(numbers: &[u64]) -> String {
    if numbers.is_empty() {
        "nothing".to_string()
    } else {
        crate::stacks::format_pr_list(numbers)
    }
}

/// Land Pull Request `pull_request_number`, with every open Pull Request
/// below it in its GitHub stack. `yes` merges those without asking. `file`
/// is the stack file being landed, if any, for the commands `land` suggests.
async fn land_pull_request(
    pull_request_number: u64,
    yes: bool,
    file: Option<&Path>,
    jj: &crate::jj::Jujutsu,
    gh: &mut crate::github::GitHub,
    config: &crate::config::Config,
) -> Result<()> {
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
    // Pull Request below it. Any other Pull Request has to target master:
    // squash-merging one based on another open Pull Request's branch would
    // merge both as one commit, and one based on any other branch (such as a
    // base branch that older versions of jj-spr made) needs `jj spr diff` to
    // retarget it.
    let stack = gh.stack_containing(pull_request_number).await?;
    if stack.is_none() && !pull_request.base.is_master_branch() {
        let base = pull_request.base.branch_name();
        let below = gh
            .get_open_pull_requests()
            .await?
            .into_iter()
            .find(|pr| pr.head_ref_name == base);
        return Err(Error::new(match below {
            Some(below) => format!(
                "Pull Request #{pull_request_number} targets {base}, the branch of \
                 open Pull Request #{below}, and is not in a GitHub stack, so landing \
                 it would merge both as one commit. Land #{below} first.",
                below = below.number
            ),
            None => format!(
                "Pull Request #{pull_request_number} targets {base}, not {master}. \
                 Run `jj spr diff` to retarget it before landing it.",
                master = config.master_ref.branch_name()
            ),
        }));
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
            confirm_merging_below(yes, &below)?;
        }
    }

    // When the repository's default squash commit for a Pull Request is its
    // title and body, one merge of the top one lands the stack: GitHub
    // squash-merges each Pull Request below it with that default, with no
    // rebase in between. Otherwise merge bottom first, one Pull Request at a
    // time, so each one gets jj-spr's title and message (GitHub applies them
    // only to the Pull Request named in the merge call). After each merge
    // GitHub retargets the next Pull Request to master and then replaces its
    // head with a rebased one, so only the first one still has the head
    // fetched above.
    let merging: Vec<&crate::github::PullRequest> =
        merged_below.iter().chain([&pull_request]).collect();
    let one_merge = stack.is_some() && gh.squash_defaults_to_pull_request().await;
    if stack.is_some() && !one_merge && merging.len() > 1 {
        output(
            "💡",
            "Setting the repository's default squash commit message to the Pull \
             Request title and description lets jj-spr land a stack in one merge.",
        )?;
    }
    let first = if one_merge { merging.len() - 1 } else { 0 };
    let mut landed: Vec<&crate::github::PullRequest> = Vec::new();
    let mut merged_sha = None;
    let mut failure = None;
    for (i, pr) in merging.iter().copied().enumerate().skip(first) {
        let head = if landed.is_empty() {
            Head::Expected(pr.head_oid)
        } else {
            Head::Replaced(pr.head_oid)
        };
        // One merge of a stack goes straight to GitHub, which checks the head
        // and every Pull Request in the merge, and says why if it refuses.
        let merged = if one_merge {
            squash_merge(gh, config, pr, pr.head_oid, true, false).await
        } else {
            match wait_until_mergeable(gh, config, pr.number, head, file).await {
                Ok(head) => squash_merge(gh, config, pr, head, stack.is_some(), true).await,
                Err(error) => Err(error),
            }
        };
        match merged {
            Ok(sha) => {
                let merged = &merging[landed.len()..=i];
                if merging.len() > 1 {
                    output("🛬", &format!("Landed {}", pr_numbers(merged)))?;
                }
                landed.extend_from_slice(merged);
                merged_sha = sha;
            }
            Err(error) => {
                failure = Some(error);
                break;
            }
        }
    }

    let failure = match failure {
        Some(error) if landed.is_empty() => {
            output("❌", "GitHub Pull Request merge failed")?;
            return Err(error);
        }
        // Each landed Pull Request is a complete commit on master, so clean
        // up after those and report the rest.
        Some(error) => {
            output(
                "❌",
                &format!(
                    "Landed {}, but not {}",
                    pr_numbers(&landed),
                    pr_numbers(&merging[landed.len()..])
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
                merged_heads
                    .iter()
                    .copied()
                    .filter(|head| !kept_heads.iter().any(|kept| kept == head.branch_name()))
                    .collect()
            }
            // Without the list, a branch that another Pull Request targets
            // cannot be told apart from an unused one, and deleting it would
            // close that Pull Request.
            Err(error) => {
                let kept: Vec<&str> = merged_heads
                    .iter()
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
            &format!(
                "Run {} to abandon the landed changes and rebase the rest of the stack",
                spr_command("sync", file)
            ),
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

/// How many polls `land` waits for GitHub to replace a Pull Request's head
/// after a merge below it, about 30 seconds. Until then GitHub may report
/// whether the old head can be merged. GitHub pushes no new head when its
/// rebase conflicts, so after this the old head's mergeability counts.
const REBASE_POLLS: u32 = 30;

/// A Pull Request's head as `land` fetched it before merging anything.
#[derive(Clone, Copy)]
enum Head {
    /// The Pull Request must still have this head.
    Expected(git2::Oid),
    /// A Pull Request merged below it in this run, and GitHub replaces this
    /// head with a rebased one.
    Replaced(git2::Oid),
}

/// Wait until GitHub has worked out whether Pull Request `number` can be
/// merged, and refuse if it cannot or GitHub blocks it. After a merge below
/// it, that is once it targets master and has a new head, or `REBASE_POLLS`
/// polls without one. Returns the head to merge.
async fn wait_until_mergeable(
    gh: &crate::github::GitHub,
    config: &crate::config::Config,
    number: u64,
    head: Head,
    file: Option<&Path>,
) -> Result<git2::Oid> {
    for poll in 0..MERGEABILITY_POLLS {
        let mergeability = gh.get_pull_request_mergeability(number).await?;
        // GitHub closes a stacked Pull Request whose rebase after a merge
        // below it leaves no commits.
        if mergeability.state == PullRequestState::Closed {
            return Err(Error::new(format!(
                "Pull Request #{number} was closed without being merged, so it \
                 cannot be landed. Abandon its change, or remove the change's \
                 `Pull Request:` line and run `jj spr diff` to open a new one."
            )));
        }
        let settled = match head {
            Head::Expected(expected) if expected != mergeability.head_oid => {
                return Err(Error::new(format!(
                    "Pull Request #{number} seems to have been updated externally. \
                     Please try again!"
                )));
            }
            Head::Expected(_) => true,
            Head::Replaced(old) => {
                mergeability.base.is_master_branch()
                    && (mergeability.head_oid != old || poll >= REBASE_POLLS)
            }
        };
        if settled && let Some(mergeable) = mergeability.mergeable {
            if !mergeable {
                return Err(Error::new(match head {
                    Head::Expected(_) => format!(
                        "GitHub concluded Pull Request #{number} is not mergeable at \
                         this point. Please rebase your changes and try again!"
                    ),
                    Head::Replaced(_) => format!(
                        "Pull Request #{number} conflicts with {} after the merge below \
                         it. Run {}, resolve the conflict, and run {} again.",
                        config.master_ref.branch_name(),
                        spr_command("sync", file),
                        spr_command(if file.is_some() { "submit" } else { "diff" }, file),
                    ),
                }));
            }
            if mergeability.blocked {
                return Err(Error::new(format!(
                    "Pull Request #{number} can't be merged yet: GitHub is blocking it \
                     (required checks or review). Run {} again when it is ready.",
                    spr_command("land", file)
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

/// Squash-merge `pull_request` at `head`, with jj-spr's title and message
/// when `own_message` is set and GitHub's default otherwise. A Pull Request
/// in a native stack has to go through GitHub's stack merge.
async fn squash_merge(
    gh: &crate::github::GitHub,
    config: &crate::config::Config,
    pull_request: &crate::github::PullRequest,
    head: git2::Oid,
    stacked: bool,
    own_message: bool,
) -> Result<Option<String>> {
    let number = pull_request.number;
    let message = build_github_body_for_merging(&pull_request.sections);
    let merged = if stacked {
        let commit = own_message.then_some((pull_request.title.as_str(), message.as_str()));
        gh.merge_stacked_pull_request(number, head, "squash", commit)
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
fn confirm_merging_below(yes: bool, below: &[u64]) -> Result<()> {
    use std::io::IsTerminal;
    if yes {
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
