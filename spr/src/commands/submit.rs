/*
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! `jj spr submit`: push the Pull Requests of a stack file.
//!
//! It checks every entry first (see `stack_file::check`), then hands the
//! entries to `diff`'s `publish` as planned Pull Requests, with the file's
//! titles and bodies. What it did goes into the stack file as each Pull
//! Request is published: `pr`, `synced`, and `pushed`.

use std::path::{Path, PathBuf};

use crate::{
    commands::diff::{Overrides, PushOptions, Record, publish},
    error::{Error, Result},
    jj::PreparedCommit,
    output::output,
    stack_file::{StackFile, base_dir, check, describe_problems},
};

#[derive(Debug, clap::Parser)]
pub struct SubmitOptions {
    /// The stack file
    path: PathBuf,

    /// Submit only the entries up to and including the one with this tip.
    /// The entries above it are not checked.
    #[clap(long, value_name = "TIP")]
    upto: Option<String>,

    /// Message for the commits that update existing Pull Requests (e.g.
    /// 'rebase' or 'review comments')
    #[clap(long, short = 'm')]
    message: Option<String>,

    /// When a title or body changed both in the stack file and on GitHub
    /// since the last submit, replace GitHub's with the file's
    #[clap(long)]
    overwrite_github: bool,

    /// Open new Pull Requests as drafts
    #[clap(long)]
    draft: bool,

    /// Push even if a Pull Request has commits that are not in its changes
    /// (someone else pushed to it). Their edits stay in the branch history
    /// but disappear from the Pull Request's diff.
    #[clap(long)]
    discard_remote_changes: bool,

    /// Show what would happen without pushing, opening Pull Requests, or
    /// writing the stack file
    #[clap(long)]
    dry_run: bool,
}

pub async fn submit(
    opts: SubmitOptions,
    jj: &crate::jj::Jujutsu,
    gh: &mut crate::github::GitHub,
    config: &crate::config::Config,
) -> Result<()> {
    let file = StackFile::read(&opts.path)?;
    let name = opts.path.display().to_string();
    let indices = file.active(opts.upto.as_deref())?;
    if indices.is_empty() {
        output("👋", "Every entry has merged. Nothing to submit.")?;
        return Ok(());
    }

    let check = check(
        jj,
        gh,
        config,
        &file,
        &base_dir(&opts.path),
        &name,
        &indices,
        opts.overwrite_github,
    )
    .await?;
    if !check.problems.is_empty() {
        return Err(Error::new(format!(
            "Not submitting: {}",
            describe_problems(&name, &check.problems)
        )));
    }

    let mut commits: Vec<PreparedCommit> = Vec::new();
    let mut overrides = Vec::new();
    let mut pull_requests = Vec::new();
    let mut record = FileRecord {
        file,
        path: &opts.path,
        dry_run: opts.dry_run,
        entries: Vec::new(),
    };
    for checked in check.entries {
        if let (Some(kept), Some(pr)) = (checked.kept_from_github(), &checked.pull_request) {
            output(
                "✋",
                &format!(
                    "{} the {kept} of #{} as edited on GitHub (unchanged in the stack \
                     file since the last submit)",
                    if opts.dry_run {
                        "Would keep"
                    } else {
                        "Keeping"
                    },
                    pr.number
                ),
            )?;
        }
        let wanted = checked.wanted.clone().expect("checked entries have a text");
        let number = checked.pull_request.as_ref().map(|pr| pr.number);
        commits.push(checked.resolved.planned(number));
        overrides.push(Overrides {
            title: Some(wanted.title.clone()),
            body: Some(wanted.body.clone()),
        });
        record.entries.push(Recorded {
            index: checked.resolved.index,
            synced: checked.wants_file_text().then(|| wanted.hash()),
        });
        pull_requests.push(checked.pull_request);
    }

    let master_base_oid = jj.get_master_base_for_commit(config, commits[0].oid)?;
    publish(
        &mut PushOptions {
            draft: opts.draft,
            message: opts.message,
            dry_run: opts.dry_run,
            discard_remote_changes: opts.discard_remote_changes,
            message_only_for_updates: true,
            ..Default::default()
        },
        jj,
        gh,
        config,
        &mut commits,
        &overrides,
        pull_requests,
        master_base_oid,
        &mut record,
    )
    .await
}

/// What `submit` writes into one entry once its Pull Request is published.
struct Recorded {
    /// The entry's index in the file.
    index: usize,
    /// The new value of `synced`, when the Pull Request gets the file's title
    /// and body.
    synced: Option<String>,
}

/// `submit`'s record: the stack file, written after each step.
struct FileRecord<'a> {
    file: StackFile,
    path: &'a Path,
    dry_run: bool,
    /// One for each planned Pull Request.
    entries: Vec<Recorded>,
}

impl Record for FileRecord<'_> {
    fn published(
        &mut self,
        index: usize,
        commit: &mut PreparedCommit,
        number: Option<u64>,
    ) -> Result<()> {
        if self.dry_run {
            return Ok(());
        }
        let recorded = &self.entries[index];
        let entry = &mut self.file.prs[recorded.index];
        let before = entry.clone();
        entry.pr = number.or(entry.pr);
        entry.pushed = Some(commit.oid.to_string());
        if let Some(synced) = &recorded.synced {
            entry.synced = Some(synced.clone());
        }
        if before == *entry {
            return Ok(());
        }
        self.file.write(self.path)
    }
}
