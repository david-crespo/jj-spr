/*
 * Copyright (c) Radical HQ Limited
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

use std::{
    path::{Path, PathBuf},
    process::Stdio,
};

use indoc::formatdoc;

use crate::{
    error::{Error, Result, add_error},
    github::{PullRequestState, PullRequestUpdate},
    jj::PreparedCommit,
    message::MessageSection,
    output::{output, write_commit_title},
    stack_file::StackFile,
};

#[derive(Debug, clap::Parser)]
pub struct CloseOptions {
    /// Close Pull Requests for commits in range from base to revision. With a
    /// stack file, close the Pull Request of every entry.
    #[clap(long, short = 'a')]
    all: bool,

    /// Base revision for --all mode (if not specified, uses trunk)
    #[clap(long)]
    base: Option<String>,

    /// Jujutsu revision(s) to operate on. Can be a single revision like '@' or a range like 'main..@' or 'a::c'.
    /// If a range is provided, behaves like --all mode. If not specified, uses '@-'.
    #[clap(short = 'r', long)]
    revision: Option<String>,

    /// A stack file: close one entry's Pull Request (--pr) or every entry's
    /// (--all, top first). The entries stay, without their pr, synced, and
    /// pushed keys, so a later jj spr submit opens new Pull Requests.
    #[clap(value_name = "STACK_FILE", conflicts_with_all = ["revision", "base"])]
    file: Option<PathBuf>,

    /// With a stack file, the tip of the entry whose Pull Request to close
    #[clap(
        long = "pr",
        value_name = "TIP",
        requires = "file",
        conflicts_with = "all"
    )]
    pr_tip: Option<String>,
}

pub async fn close(
    opts: CloseOptions,
    jj: &crate::jj::Jujutsu,
    gh: &mut crate::github::GitHub,
    config: &crate::config::Config,
) -> Result<()> {
    if let Some(path) = &opts.file {
        return close_stack_file(&opts, path, jj, gh, config).await;
    }
    let mut result = Ok(());

    // Determine revision and whether to use range mode
    let (use_range_mode, base_rev, target_rev, is_inclusive) =
        crate::revision_utils::parse_revision_and_range(
            opts.revision.as_deref(),
            opts.all,
            opts.base.as_deref(),
        )?;

    let mut prepared_commits = if use_range_mode {
        jj.get_prepared_commits_from_to(config, &base_rev, &target_rev, is_inclusive)?
    } else {
        vec![jj.get_prepared_commit_for_revision(config, &target_rev)?]
    };

    if prepared_commits.is_empty() {
        output("👋", "No commits found - nothing to do. Good bye!")?;
        return result;
    }

    let selection: Vec<u64> = prepared_commits
        .iter()
        .filter_map(|pc| pc.pull_request_number)
        .collect();

    for prepared_commit in prepared_commits.iter_mut() {
        if result.is_err() {
            break;
        }

        write_commit_title(prepared_commit)?;

        // The further implementation of the close command is in a separate function.
        // This makes it easier to run the code to update the local commit message
        // with all the changes that the implementation makes at the end, even if
        // the implementation encounters an error or exits early.
        result = close_impl(jj, gh, config, prepared_commit, &selection).await;
    }

    // This updates the commit message in the local Jujutsu repository (if it was
    // changed by the implementation)
    add_error(
        &mut result,
        jj.rewrite_commit_messages(config, &mut prepared_commits),
    );

    result
}

/// Close the Pull Requests of a stack file's entries, and clear their `pr`,
/// `synced`, and `pushed`.
async fn close_stack_file(
    opts: &CloseOptions,
    path: &Path,
    jj: &crate::jj::Jujutsu,
    gh: &mut crate::github::GitHub,
    config: &crate::config::Config,
) -> Result<()> {
    let mut file = StackFile::read(path)?;
    let targets: Vec<usize> = match (&opts.pr_tip, opts.all) {
        (Some(tip), _) => {
            let index = file.find(tip)?;
            if file.prs[index].pr.is_none() || file.prs[index].merged {
                return Err(Error::new(format!(
                    "'{tip}' has no open Pull Request to close."
                )));
            }
            vec![index]
        }
        (None, true) => (0..file.prs.len())
            .filter(|&i| file.prs[i].pr.is_some() && !file.prs[i].merged)
            .collect(),
        (None, false) => {
            return Err(Error::new(
                "With a stack file, pass --pr <tip> to close one entry's Pull Request, or \
                 --all to close every one.",
            ));
        }
    };
    let selection: Vec<u64> = targets.iter().filter_map(|&i| file.prs[i].pr).collect();
    let resubmit = format!("`jj spr submit {}`", path.display());
    // Top first: deleting a branch closes the Pull Requests that target it.
    for index in targets.into_iter().rev() {
        let entry = &mut file.prs[index];
        let number = entry.pr.expect("only entries with a Pull Request");
        output("📄", &entry.tip)?;
        close_pull_request(jj, gh, config, number, &selection, Some(&resubmit)).await?;
        entry.pr = None;
        entry.synced = None;
        entry.pushed = None;
        file.write(path)?;
    }
    Ok(())
}

async fn close_impl(
    jj: &crate::jj::Jujutsu,
    gh: &mut crate::github::GitHub,
    config: &crate::config::Config,
    prepared_commit: &mut PreparedCommit,
    selection: &[u64],
) -> Result<()> {
    let Some(pull_request_number) = prepared_commit.pull_request_number else {
        return Err(Error::new("This commit does not refer to a Pull Request."));
    };
    close_pull_request(jj, gh, config, pull_request_number, selection, None).await?;

    // Remove sections from commit that are not relevant after closing.
    prepared_commit.message.remove(&MessageSection::PullRequest);
    prepared_commit.message.remove(&MessageSection::ReviewedBy);
    prepared_commit.message_changed = true;
    Ok(())
}

/// Close Pull Request `pull_request_number` and delete its branch, unless
/// another open Pull Request outside `selection` (the ones this run closes)
/// targets it. `resubmit` is the command that restacks those, for a stack
/// file.
async fn close_pull_request(
    jj: &crate::jj::Jujutsu,
    gh: &mut crate::github::GitHub,
    config: &crate::config::Config,
    pull_request_number: u64,
    selection: &[u64],
    resubmit: Option<&str>,
) -> Result<()> {
    output("#️⃣ ", &format!("Pull Request #{}", pull_request_number))?;

    // Load Pull Request information
    let pull_request = gh.clone().get_pull_request(pull_request_number).await?;

    if pull_request.state != PullRequestState::Open {
        return Err(Error::new(formatdoc!(
            "This Pull Request is already closed!",
        )));
    }

    output("📖", "Getting started...")?;

    let base_is_master = pull_request.base.is_master_branch();

    let result = gh
        .update_pull_request(
            pull_request_number,
            PullRequestUpdate {
                state: Some(PullRequestState::Closed),
                ..Default::default()
            },
        )
        .await;

    match result {
        Ok(()) => (),
        Err(error) => {
            output("❌", "GitHub Pull Request close failed")?;

            return Err(error);
        }
    };

    output("📕", "Closed!")?;

    // Keep branches that other open Pull Requests still use: GitHub closes a
    // Pull Request whose base branch is deleted, and a natively stacked Pull
    // Request's base is another Pull Request's branch.
    let open_pull_requests = gh.get_open_pull_requests().await?;
    let dependents: Vec<u64> = open_pull_requests
        .iter()
        .filter(|pr| {
            pr.number != pull_request_number
                && !selection.contains(&pr.number)
                && pr.base_ref_name == pull_request.head.branch_name()
        })
        .map(|pr| pr.number)
        .collect();
    let base_used_elsewhere = open_pull_requests.iter().any(|pr| {
        pr.number != pull_request_number
            && (pr.head_ref_name == pull_request.base.branch_name()
                || pr.base_ref_name == pull_request.base.branch_name())
    });

    let remove_old_branch_child_process = if dependents.is_empty() {
        Some(
            jj.git_command()
                .arg("push")
                .arg("--no-verify")
                .arg("--delete")
                .arg("--")
                .arg(&config.remote_name)
                .arg(pull_request.head.on_github())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()?,
        )
    } else {
        let (targets, changes, them) = if dependents.len() == 1 {
            ("targets", "its change", "it")
        } else {
            ("target", "those changes", "them")
        };
        let restack = match resubmit {
            Some(resubmit) => format!("Run {resubmit} to restack {them}."),
            None => format!("Rebase {changes} and run `jj spr diff --all` to restack {them}."),
        };
        output(
            "⚠️",
            &format!(
                "Keeping branch {} because {} still {targets} it. {restack}",
                pull_request.head.branch_name(),
                crate::stacks::format_pr_list(&dependents),
            ),
        )?;
        None
    };

    let remove_old_base_branch_child_process = if base_is_master || base_used_elsewhere {
        None
    } else {
        Some(
            jj.git_command()
                .arg("push")
                .arg("--no-verify")
                .arg("--delete")
                .arg("--")
                .arg(&config.remote_name)
                .arg(pull_request.base.on_github())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()?,
        )
    };

    // Wait for the "git push" to delete the old Pull Request branch to finish,
    // but ignore the result.
    // GitHub may be configured to delete the branch automatically,
    // in which case it's gone already and this command fails.
    if let Some(mut proc) = remove_old_branch_child_process {
        proc.wait().await?;
    }
    if let Some(mut proc) = remove_old_base_branch_child_process {
        proc.wait().await?;
    }

    Ok(())
}
