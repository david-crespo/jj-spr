/*
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! `jj spr stack-file`: write a new stack file, show what `jj spr submit`
//! would do with one, and adopt Pull Requests opened without jj-spr. Agents
//! and people edit the file itself; these keep them from working blind.

use std::{collections::HashMap, path::PathBuf};

use git2::Oid;
use serde_json::{Value, json};

use crate::{
    error::{Error, Result},
    github::PullRequestState,
    jj::short,
    output::output,
    stack_file::{Check, Entry, Member, StackFile, Text, base_dir, check, quote},
};

#[derive(Debug, clap::Subcommand)]
pub enum StackFileCommand {
    /// Write a new stack file with one entry per tip
    Init(InitOptions),

    /// Check a stack file and show what `jj spr submit` would do
    ///
    /// Shows each entry with its changes, title, and Pull Request, and what
    /// submit would do with it. For an entry with `pushed`, it shows which
    /// changes are new, edited, or reworded since the last submit. Lists
    /// every problem that would stop submit, and exits with an error when
    /// there is one.
    Status(StatusOptions),

    /// Take over Pull Requests opened without jj-spr
    ///
    /// For each entry without a pr key whose tip is a local bookmark, adopt
    /// finds the open Pull Request in this repository whose branch has the
    /// bookmark's name. It writes the entry's pr, pushed, and synced keys,
    /// so that submit updates that Pull Request instead of opening a new
    /// one. It changes nothing on GitHub. Since synced has the entry's
    /// current title and body, submit keeps GitHub's where they differ.
    ///
    /// It stops tracking the bookmark's remote branch, since jj-spr adds
    /// commits to that branch from then on, and rewrites the changes on that
    /// branch so that they stay editable.
    Adopt(AdoptOptions),
}

#[derive(Debug, clap::Parser)]
pub struct InitOptions {
    /// Where to write the stack file. It must not exist yet.
    path: PathBuf,

    /// The top change of each Pull Request, as a revset. The tips have to be
    /// in a line, each an ancestor of the next. An entry's tip is the
    /// change's local bookmark when it has exactly one, or its change ID
    /// otherwise.
    #[clap(long, value_name = "REVSET")]
    tips: String,
}

#[derive(Debug, clap::Parser)]
pub struct StatusOptions {
    /// The stack file
    path: PathBuf,

    /// Print the same as JSON
    #[clap(long)]
    json: bool,
}

#[derive(Debug, clap::Parser)]
pub struct AdoptOptions {
    /// The stack file
    path: PathBuf,

    /// Write each adopted Pull Request's title and body from GitHub into the
    /// entry's body file, replacing what the file has
    #[clap(long)]
    copy_github_text: bool,

    /// Show what would happen without writing files, changing bookmarks, or
    /// rewriting changes
    #[clap(long)]
    dry_run: bool,
}

pub fn init(opts: InitOptions, jj: &crate::jj::Jujutsu) -> Result<()> {
    let mut tips = jj.commit_ids(&opts.tips)?;
    tips.reverse();
    if tips.is_empty() {
        return Err(Error::new(format!(
            "--tips '{}' has no changes.",
            opts.tips
        )));
    }
    for pair in tips.windows(2) {
        if !jj.git_repo.graph_descendant_of(pair[1], pair[0])? {
            return Err(Error::new(format!(
                "The tips have to be in a line, each an ancestor of the next, but \
                 {:.7} is not an ancestor of {:.7}.",
                pair[0], pair[1]
            )));
        }
    }

    let revset = tips
        .iter()
        .map(Oid::to_string)
        .collect::<Vec<_>>()
        .join(" | ");
    let listing = jj.run([
        "log",
        "--no-graph",
        "-r",
        &revset,
        "-T",
        "commit_id ++ \"\\t\" ++ change_id.short() ++ \"\\t\" ++ \
         local_bookmarks.map(|b| b.name()).join(\" \") ++ \"\\t\" ++ \
         description.first_line() ++ \"\\n\"",
    ])?;
    let mut entries: Vec<(Oid, Entry)> = Vec::new();
    for line in listing.lines() {
        let fields: Vec<&str> = line.splitn(4, '\t').collect();
        let [commit, change_id, bookmarks, title] = fields[..] else {
            continue;
        };
        let bookmarks: Vec<&str> = bookmarks.split_whitespace().collect();
        let tip = match bookmarks[..] {
            [bookmark] => bookmark.to_string(),
            _ => change_id.to_string(),
        };
        let title = title.trim();
        entries.push((
            Oid::from_str(commit)?,
            Entry {
                tip,
                title: (!title.is_empty()).then(|| title.to_string()),
                ..Default::default()
            },
        ));
    }
    let file = StackFile {
        prs: tips
            .iter()
            .filter_map(|tip| entries.iter().find(|(c, _)| c == tip))
            .map(|(_, entry)| entry.clone())
            .collect(),
        ..Default::default()
    };
    file.create(&opts.path)?;
    println!(
        "Wrote {} with {} entries, bottom first:",
        opts.path.display(),
        file.prs.len()
    );
    for entry in &file.prs {
        println!(
            "  {}  {}",
            entry.tip,
            entry.title.as_deref().unwrap_or("(no title)")
        );
    }
    Ok(())
}

pub async fn status(
    opts: StatusOptions,
    jj: &crate::jj::Jujutsu,
    gh: &crate::github::GitHub,
    config: &crate::config::Config,
) -> Result<()> {
    let file = StackFile::read(&opts.path)?;
    let name = opts.path.display().to_string();
    let indices = file.active(None)?;
    let check = check(
        jj,
        gh,
        config,
        &file,
        &base_dir(&opts.path),
        &name,
        &indices,
        false,
    )
    .await?;

    let rows: Vec<Value> = file
        .prs
        .iter()
        .enumerate()
        .map(|(index, entry)| row(jj, config, &check, index, entry))
        .collect::<Result<_>>()?;
    let problems = &check.problems;

    if opts.json {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "file": name,
                "entries": rows,
                "problems": problems,
            }))?
        );
        return if problems.is_empty() {
            Ok(())
        } else {
            Err(Error::empty())
        };
    }

    println!("{name}: {} entries, bottom first", rows.len());
    for (position, row) in rows.iter().enumerate() {
        print_row(position + 1, row);
    }
    if problems.is_empty() {
        println!("\nNo problems: `jj spr submit {name}` would go ahead.");
        return Ok(());
    }
    println!("\nProblems (`jj spr submit` refuses until they are fixed):");
    for problem in problems {
        let mut lines = problem.message.lines();
        println!("  - {}", lines.next().unwrap_or_default());
        for line in lines {
            println!("    {line}");
        }
    }
    Err(Error::new(format!(
        "{name} has {} problem{}.",
        problems.len(),
        if problems.len() == 1 { "" } else { "s" }
    )))
}

pub async fn adopt(
    opts: AdoptOptions,
    jj: &crate::jj::Jujutsu,
    gh: &crate::github::GitHub,
    config: &crate::config::Config,
) -> Result<()> {
    let mut file = StackFile::read(&opts.path)?;
    let dir = base_dir(&opts.path);
    let remote = &config.remote_name;
    // The tips of the entries adopted in this run.
    let mut adopted = Vec::new();
    for index in file.active(None)? {
        let entry = &file.prs[index];
        if entry.pr.is_some() {
            continue;
        }
        let tip = entry.tip.clone();
        if jj
            .commit_ids(&format!("bookmarks(exact:{})", quote(&tip)))?
            .len()
            != 1
        {
            output(
                "➖",
                &format!("'{tip}' is not a local bookmark, so it has no branch to adopt."),
            )?;
            continue;
        }
        // What was last pushed, read before fetching the Pull Request moves
        // the remote-tracking branch to GitHub's head.
        let pushed = jj
            .resolve_reference(config.new_github_branch(&tip).local())
            .ok();
        let found = gh.get_pull_requests_for_branch(&tip).await?;
        let open: Vec<u64> = found
            .iter()
            .filter(|(_, state)| *state == PullRequestState::Open)
            .map(|(number, _)| *number)
            .collect();
        let number = match open[..] {
            [number] => number,
            [] => {
                for (number, state) in &found {
                    let what = match state {
                        PullRequestState::Merged => "has merged",
                        _ => "is closed",
                    };
                    output("➖", &format!("#{number}, from {tip}, {what}."))?;
                }
                output(
                    "➖",
                    &format!(
                        "{}Leaving '{tip}' alone: the next submit opens a new Pull Request for it.",
                        if found.is_empty() {
                            format!("No Pull Request has {tip} as its branch. ")
                        } else {
                            String::new()
                        }
                    ),
                )?;
                continue;
            }
            _ => {
                output(
                    "⚠️",
                    &format!(
                        "{} are open from {tip}. Leaving '{tip}' alone: set the entry's pr \
                         to the right one.",
                        crate::stacks::format_pr_list(&open)
                    ),
                )?;
                continue;
            }
        };
        let pr = gh.get_pull_request(number).await?;
        let github = Text::new(&pr.title, pr.body.as_deref().unwrap_or_default());
        output(
            "#️⃣ ",
            &format!(
                "{} #{number} for '{tip}': {}",
                if opts.dry_run {
                    "Would adopt"
                } else {
                    "Adopted"
                },
                config.pull_request_url(number)
            ),
        )?;

        let mut text = entry.text(&dir).map(|(text, _)| text);
        match (&entry.body, opts.copy_github_text) {
            (Some(body), true) => {
                let markdown = match github.body.as_str() {
                    "" => format!("# {}\n", github.title),
                    body => format!("# {}\n\n{body}\n", github.title),
                };
                let path = dir.join(body);
                if !opts.dry_run {
                    std::fs::write(&path, &markdown).map_err(|e| {
                        Error::new(format!("Could not write {}: {e}", path.display()))
                    })?;
                }
                output(
                    "📝",
                    &format!(
                        "{} GitHub's title and body to {}",
                        if opts.dry_run { "Would copy" } else { "Copied" },
                        path.display()
                    ),
                )?;
                text = entry.text_from(&markdown).map(|(text, _)| text);
            }
            (None, true) => output(
                "⚠️",
                &format!(
                    "The entry of '{tip}' has no body file to copy GitHub's title and body to. \
                     Add a body key and copy them by hand, since adopt now skips the entry."
                ),
            )?,
            (_, false) => {}
        }
        // The file's text as `synced`: submit keeps GitHub's while the file
        // stays as it is.
        let synced = match &text {
            Ok(text) => {
                let differ = match (text.title != github.title, text.body != github.body) {
                    (false, false) => None,
                    (true, false) => Some("title differs"),
                    (false, true) => Some("body differs"),
                    (true, true) => Some("title and body differ"),
                };
                match differ {
                    None => output("✅", "The title and body match GitHub's.")?,
                    Some(differ) => output(
                        "✋",
                        &format!(
                            "The {differ} from GitHub's. The next submit keeps GitHub's \
                             unless it gets --overwrite-github."
                        ),
                    )?,
                }
                Some(text.hash())
            }
            Err(message) => {
                output("⚠️", message)?;
                None
            }
        };
        if pushed.is_none() {
            output(
                "⚠️",
                &format!(
                    "There is no remote-tracking bookmark {tip}@{remote}, so the entry gets \
                     no pushed key."
                ),
            )?;
        }
        let entry = &mut file.prs[index];
        entry.pr = Some(number);
        entry.pushed = pushed.map(|oid| oid.to_string());
        entry.synced = synced;
        adopted.push(tip);
    }
    if !adopted.is_empty() && !opts.dry_run {
        file.write(&opts.path)?;
    }
    untrack_adopted(jj, config, &adopted, opts.dry_run)
}

/// Stop tracking the remote branches of the bookmarks `adopted`, the tips of
/// adopted entries. jj-spr adds commits to the branch, and a tracked bookmark
/// would move to those commits, or become conflicted, on the next fetch.
///
/// By default, jj treats the changes on an untracked remote branch as
/// immutable. The changes that untracking makes immutable are rewritten with
/// the same content: the new commits are editable, and the old ones are
/// hidden.
fn untrack_adopted(
    jj: &crate::jj::Jujutsu,
    config: &crate::config::Config,
    adopted: &[String],
    dry_run: bool,
) -> Result<()> {
    let remote = &config.remote_name;
    let mut bookmarks = Vec::new();
    for tip in adopted {
        let listing = jj.run([
            "bookmark",
            "list",
            "--all-remotes",
            "-T",
            &format!("if(remote == {}, tracked ++ \"\\n\")", quote(remote)),
            &format!("exact:{tip}"),
        ])?;
        if listing.trim() == "true" {
            bookmarks.push(tip.as_str());
        }
    }
    if bookmarks.is_empty() {
        return Ok(());
    }
    let union = |f: &dyn Fn(&str) -> String| {
        bookmarks
            .iter()
            .map(|b| f(b))
            .collect::<Vec<_>>()
            .join(" | ")
    };
    let tips = union(&|b| format!("bookmarks(exact:{})", quote(b)));
    let remotes = union(&|b| {
        format!(
            "remote_bookmarks(exact:{}, exact:{})",
            quote(b),
            quote(remote)
        )
    });
    let pushed = jj.commit_ids(&format!(
        "({}..({tips})) & ::({remotes}) & mutable()",
        config.trunk_revset()
    ))?;
    let changes = |n: usize| format!("{n} change{}", if n == 1 { "" } else { "s" });
    if dry_run {
        for bookmark in &bookmarks {
            output("🔖", &format!("Would untrack {bookmark}@{remote}"))?;
        }
        if !pushed.is_empty() {
            output(
                "♻️ ",
                &format!(
                    "Would rewrite {} with the same content if untracking makes them \
                     immutable, as it does by default",
                    changes(pushed.len())
                ),
            )?;
        }
        return Ok(());
    }

    let mut untrack = vec![
        "bookmark".to_string(),
        "untrack".to_string(),
        "--quiet".to_string(),
        "--remote".to_string(),
        remote.clone(),
    ];
    untrack.extend(bookmarks.iter().map(|b| format!("exact:{b}")));
    jj.run(&untrack)?;
    for bookmark in &bookmarks {
        output(
            "🔖",
            &format!(
                "Untracked {bookmark}@{remote}, so that {bookmark} stays on your changes \
                 when jj-spr adds commits to the branch."
            ),
        )?;
    }

    let revset = |oids: &[Oid]| {
        oids.iter()
            .map(Oid::to_string)
            .collect::<Vec<_>>()
            .join(" | ")
    };
    if pushed.is_empty() {
        return Ok(());
    }
    let now_immutable = jj.commit_ids(&format!("({}) & immutable()", revset(&pushed)))?;
    if now_immutable.is_empty() {
        return Ok(());
    }
    jj.run([
        "metaedit",
        "--force-rewrite",
        "--ignore-immutable",
        "--quiet",
        &revset(&now_immutable),
    ])?;
    output(
        "♻️ ",
        &format!(
            "Rewrote {} with the same content, so that they stay editable: jj treats \
             the changes on an untracked remote branch as immutable.",
            changes(now_immutable.len())
        ),
    )
}

/// One entry for `status`, as JSON.
fn row(
    jj: &crate::jj::Jujutsu,
    config: &crate::config::Config,
    check: &Check,
    index: usize,
    entry: &Entry,
) -> Result<Value> {
    let mut row = json!({"tip": entry.tip, "pr": entry.pr, "merged": entry.merged});
    let Some(position) = check.entries.iter().position(|c| c.resolved.index == index) else {
        return Ok(row);
    };
    let checked = &check.entries[position];
    let members = &checked.resolved.members;
    let since = match &entry.pushed {
        Some(pushed) => {
            row["pushed"] = json!(pushed);
            Some(since_pushed(jj, config, pushed, members)?)
        }
        None => None,
    };
    let members: Vec<Value> = members
        .iter()
        .enumerate()
        .map(|(i, m)| {
            let mut member = json!({"change_id": short(&m.change_id), "title": m.title()});
            if let Some(since) = &since {
                member["since_pushed"] = json!(since.as_ref().map_or("unknown", |s| s[i]));
            }
            member
        })
        .collect();
    row["members"] = json!(members);
    if let Some((text, source)) = &checked.text {
        row["title"] = json!(text.title);
        row["title_from"] = json!(source.describe());
    }
    if let Some(pr) = &checked.pull_request {
        row["state"] = json!(pr.state);
    }
    row["actions"] = json!(actions(jj, config, check, position)?);
    Ok(row)
}

/// What changed in each of `members` since `pushed`, the tip that `submit`
/// last pushed: "new", "edited", "reworded", or "unchanged". A change's old
/// version is the ancestor of `pushed` with its change ID. A change on a new
/// parent with the same diff is unchanged. `None` when `pushed` is not in
/// the repository.
fn since_pushed(
    jj: &crate::jj::Jujutsu,
    config: &crate::config::Config,
    pushed: &str,
    members: &[Member],
) -> Result<Option<Vec<&'static str>>> {
    let Some(pushed) = Oid::from_str(pushed)
        .ok()
        .filter(|&oid| jj.tree_if_present(oid).is_some())
    else {
        return Ok(None);
    };
    let Ok(listing) = jj.run([
        "log",
        "--no-graph",
        "-r",
        &format!("::{pushed} ~ ::{}", config.trunk_revset()),
        "-T",
        "change_id ++ \" \" ++ commit_id ++ \"\\n\"",
    ]) else {
        return Ok(None);
    };
    let old: HashMap<&str, Oid> = listing
        .lines()
        .filter_map(|line| {
            let (change_id, commit) = line.split_once(' ')?;
            Some((change_id, Oid::from_str(commit).ok()?))
        })
        .collect();
    let repo = &jj.git_repo;
    let mut since = Vec::new();
    for member in members {
        let Some(&old) = old.get(member.change_id.as_str()) else {
            since.push("new");
            continue;
        };
        let (old, new) = (repo.find_commit(old)?, repo.find_commit(member.commit.oid)?);
        let (old_parent, new_parent) = (old.parent(0)?.tree_id(), new.parent(0)?.tree_id());
        // Each diff, applied to the other's parent, gives the other's tree.
        let same_diff = jj.merge_trees(old_parent, new_parent, old.tree_id())?
            == Some(new.tree_id())
            && jj.merge_trees(new_parent, old_parent, new.tree_id())? == Some(old.tree_id());
        since.push(if !same_diff {
            "edited"
        } else if old.message_bytes() != new.message_bytes() {
            "reworded"
        } else {
            "unchanged"
        });
    }
    Ok(Some(since))
}

/// What `submit` would do for the entry at `position` in `check`. Nothing
/// when the changes of this entry or one below it are not known, or one of
/// them has a Pull Request that is not open: what it would do then depends
/// on how the problem is fixed.
fn actions(
    jj: &crate::jj::Jujutsu,
    config: &crate::config::Config,
    check: &Check,
    position: usize,
) -> Result<Vec<String>> {
    let unknown = check.entries[..=position].iter().any(|c| {
        c.resolved.members.is_empty()
            || c.pull_request
                .as_ref()
                .is_some_and(|pr| pr.state != PullRequestState::Open)
    });
    if unknown {
        return Ok(Vec::new());
    }
    let checked = &check.entries[position];
    let Some(pr) = &checked.pull_request else {
        return Ok(vec!["open a Pull Request".to_string()]);
    };
    let mut actions = Vec::new();
    let tip_tree = checked
        .resolved
        .tip
        .map(|tip| jj.get_tree_oid_for_commit(tip))
        .transpose()?;
    if tip_tree.is_some() && tip_tree != jj.tree_if_present(pr.head_oid) {
        actions.push("push the changes".to_string());
    }
    let expected_base = match position.checked_sub(1) {
        None => Some(config.master_ref.branch_name().to_string()),
        Some(below) => check.entries[below]
            .pull_request
            .as_ref()
            .map(|below| below.head.branch_name().to_string()),
    };
    if expected_base.as_deref() != Some(pr.base.branch_name()) {
        actions.push(format!(
            "retarget it from {} to {}",
            pr.base.branch_name(),
            expected_base
                .as_deref()
                .unwrap_or("the new Pull Request below"),
        ));
    }
    let github = crate::stack_file::Text::new(&pr.title, pr.body.as_deref().unwrap_or_default());
    if checked.wanted.as_ref().is_some_and(|w| *w != github) {
        actions.push("update the title and body".to_string());
    }
    if let Some(kept) = checked.kept_from_github() {
        actions.push(format!("keep the {kept} edited on GitHub"));
    }
    if actions.is_empty() {
        actions.push("nothing: it is up to date".to_string());
    }
    Ok(actions)
}

fn print_row(position: usize, row: &Value) {
    let text = |key: &str| row[key].as_str().unwrap_or_default().to_string();
    let pr = match (row["pr"].as_u64(), row["state"].as_str()) {
        (Some(n), Some(state)) => format!("  #{n} ({state})"),
        (Some(n), None) => format!("  #{n}"),
        _ => String::new(),
    };
    if row["merged"].as_bool() == Some(true) {
        println!("\n{position}. {}{pr}  merged", text("tip"));
        return;
    }
    println!("\n{position}. {}{pr}", text("tip"));
    if row["title"].is_string() {
        println!("   Title: {} (from {})", text("title"), text("title_from"));
    }
    let members = row["members"].as_array().cloned().unwrap_or_default();
    let since = |member: &Value| member["since_pushed"].as_str().map(str::to_string);
    for (i, member) in members.iter().enumerate() {
        let changed = match since(member).as_deref() {
            Some(what @ ("new" | "edited" | "reworded")) => {
                format!(" ({what} since the last submit)")
            }
            _ => String::new(),
        };
        println!(
            "   {} {} {}{changed}",
            if i == 0 { "Changes:" } else { "        " },
            member["change_id"].as_str().unwrap_or_default(),
            member["title"].as_str().unwrap_or_default()
        );
    }
    let since: Vec<String> = members.iter().filter_map(since).collect();
    if !since.is_empty() && since.iter().all(|s| s == "unknown") {
        println!(
            "   The last pushed commit, {:.12}, is not in this repository, so what changed \
             since then is unknown.",
            text("pushed")
        );
    } else if !since.is_empty() && since.iter().all(|s| s == "unchanged") {
        println!("   No local changes since the last submit");
    }
    if let Some(actions) = row["actions"].as_array() {
        let actions: Vec<&str> = actions.iter().filter_map(Value::as_str).collect();
        if !actions.is_empty() {
            println!("   Submit would: {}", actions.join(", "));
        }
    }
}
