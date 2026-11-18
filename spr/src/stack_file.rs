/*
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Stack files.
//!
//! A stack file lists a stack's Pull Requests, bottom first, before any of
//! them exist: where each one ends, and its title and body. `jj spr submit`
//! pushes them and writes back what it did. Nothing is written into change
//! descriptions.
//!
//! ```json
//! {
//!   "prs": [
//!     { "tip": "diff-paths", "body": "diff-paths.md", "pr": 51,
//!       "synced": "sha256:9f2c…" },
//!     { "tip": "kxqzwmvt", "title": "Run jj and git in the diff's directory" }
//!   ]
//! }
//! ```
//!
//! An entry's changes run from its tip down to the tip of the entry below,
//! or to trunk for the bottom entry. jj-spr keeps keys it doesn't know, and
//! always writes the file in one layout.

use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

use git2::Oid;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{
    config::Config,
    error::{Error, Result},
    github::{GitHub, PullRequest, PullRequestState},
    jj::{Jujutsu, PreparedCommit, short},
    message::MessageSection,
    stacks::format_pr_list,
};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct StackFile {
    /// The Pull Requests, bottom first.
    pub prs: Vec<Entry>,
    /// Keys jj-spr does not know, kept as they are.
    #[serde(flatten)]
    pub other: BTreeMap<String, Value>,
}

/// One Pull Request of a stack file. People and agents write `tip`,
/// `title`, and `body`; jj-spr writes the rest. The fields are in the order
/// they are written.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Entry {
    /// A bookmark or a change ID: the Pull Request's top change.
    pub tip: String,
    /// The title, used when there is no body file or it has no heading.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// A markdown file, relative to the stack file, with the body. A leading
    /// `#` heading is the title.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pr: Option<u64>,
    /// The hash of the title and body last pushed (see `Text::hash`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub synced: Option<String>,
    /// The local commit at the tip when `submit` last pushed the branch or
    /// found it up to date, for comparing the changes with what was pushed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pushed: Option<String>,
    /// Set by `jj spr sync` once the Pull Request merged. Every command
    /// skips merged entries.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub merged: bool,
    #[serde(flatten)]
    pub other: BTreeMap<String, Value>,
}

impl StackFile {
    pub fn read(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path)
            .map_err(|e| Error::new(format!("Could not read {}: {e}", path.display())))?;
        Self::parse(&text)
            .map_err(|e| Error::new(format!("{} is not a valid stack file: {e}", path.display())))
    }

    fn parse(text: &str) -> serde_json::Result<Self> {
        serde_json::from_str(text)
    }

    /// The file's text: two-space indent, and the keys of each entry in a
    /// fixed order, with unknown keys after them.
    pub fn to_text(&self) -> String {
        let mut text = serde_json::to_string_pretty(self).expect("a stack file serializes");
        text.push('\n');
        text
    }

    /// Replace the file at `path`. A rename, so that a failure leaves the old
    /// file whole.
    pub fn write(&self, path: &Path) -> Result<()> {
        let name = path.file_name().unwrap_or_default().to_string_lossy();
        let temporary = path.with_file_name(format!(".{name}.tmp"));
        std::fs::write(&temporary, self.to_text())
            .and_then(|()| std::fs::rename(&temporary, path))
            .map_err(|e| Error::new(format!("Could not write {}: {e}", path.display())))
    }

    /// Write a new file at `path`, refusing to replace one.
    pub fn create(&self, path: &Path) -> Result<()> {
        use std::io::Write;
        if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
            std::fs::create_dir_all(dir)?;
        }
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(path)
            .map_err(|e| match e.kind() {
                std::io::ErrorKind::AlreadyExists => {
                    Error::new(format!("{} already exists.", path.display()))
                }
                _ => Error::new(format!("Could not create {}: {e}", path.display())),
            })?;
        file.write_all(self.to_text().as_bytes())?;
        Ok(())
    }

    /// The index of the entry whose tip is `tip`, as written in the file.
    pub fn find(&self, tip: &str) -> Result<usize> {
        self.prs.iter().position(|e| e.tip == tip).ok_or_else(|| {
            Error::new(format!(
                "No entry has the tip '{tip}'. The tips are: {}.",
                self.prs
                    .iter()
                    .map(|e| e.tip.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ))
        })
    }

    /// The entries a command works on, bottom first: those not merged, up to
    /// and including the one whose tip is `upto`.
    pub fn active(&self, upto: Option<&str>) -> Result<Vec<usize>> {
        let last = match upto {
            Some(tip) => {
                let index = self.find(tip)?;
                if self.prs[index].merged {
                    return Err(Error::new(format!("The entry '{tip}' has merged already.")));
                }
                index
            }
            None => self.prs.len().saturating_sub(1),
        };
        Ok((0..self.prs.len())
            .filter(|&i| i <= last && !self.prs[i].merged)
            .collect())
    }
}

/// The directory that body paths are relative to: the stack file's.
pub fn base_dir(path: &Path) -> PathBuf {
    match path.parent() {
        Some(dir) if !dir.as_os_str().is_empty() => dir.to_path_buf(),
        _ => PathBuf::from("."),
    }
}

/// A Pull Request's title and body, normalized so they compare equal to
/// GitHub's copy: `\n` line endings, a trimmed title, and a body without
/// blank lines at the start or whitespace at the end.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Text {
    pub title: String,
    pub body: String,
}

impl Text {
    pub fn new(title: &str, body: &str) -> Self {
        let body = body.replace("\r\n", "\n");
        let mut body = body.trim_end();
        while let Some((line, rest)) = body.split_once('\n')
            && line.trim().is_empty()
        {
            body = rest;
        }
        Self {
            title: title.trim().to_string(),
            body: body.to_string(),
        }
    }

    /// The value of `synced` for this title and body.
    pub fn hash(&self) -> String {
        use sha2::Digest;
        let both = serde_json::to_string(&[&self.title, &self.body]).expect("strings serialize");
        let digest = sha2::Sha256::digest(both.as_bytes());
        let hex: String = digest.iter().map(|b| format!("{b:02x}")).collect();
        format!("sha256:{hex}")
    }
}

/// Where an entry's title comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TitleSource {
    /// The `#` heading of the body file.
    Heading,
    /// The entry's `title` key.
    Key,
}

impl TitleSource {
    pub fn describe(self) -> &'static str {
        match self {
            TitleSource::Heading => "the body file's heading",
            TitleSource::Key => "the title key",
        }
    }
}

impl Entry {
    /// The entry's title and body: the body file, whose leading `#` heading
    /// is the title, with the `title` key when it has no heading. Without a
    /// body file, the body is empty. `dir` is the stack file's directory.
    pub fn text(&self, dir: &Path) -> std::result::Result<(Text, TitleSource), String> {
        let markdown = match &self.body {
            Some(body) => {
                let path = dir.join(body);
                std::fs::read_to_string(&path).map_err(|e| {
                    format!(
                        "The body file of '{}', {}, could not be read: {e}",
                        self.tip,
                        path.display()
                    )
                })?
            }
            None => String::new(),
        };
        let (heading, body) = split_heading(&markdown);
        let (title, source) = match (heading, &self.title) {
            (Some(heading), _) => (heading, TitleSource::Heading),
            (None, Some(title)) if !title.trim().is_empty() => (title.as_str(), TitleSource::Key),
            _ => {
                return Err(format!(
                    "'{}' has no title. Give it a title key, or start its body file with \
                     a `# ` heading.",
                    self.tip
                ));
            }
        };
        Ok((Text::new(title, body), source))
    }
}

/// Split a leading `# ` heading off markdown: the heading's text, and the
/// rest. Blank lines before the heading are allowed.
fn split_heading(markdown: &str) -> (Option<&str>, &str) {
    let start = markdown.trim_start();
    if let Some(rest) = start.strip_prefix("# ") {
        let (line, body) = rest.split_once('\n').unwrap_or((rest, ""));
        let heading = line.trim().trim_end_matches('#').trim_end();
        if !heading.is_empty() {
            return (Some(heading), body);
        }
    }
    (None, markdown)
}

/// Something `submit` refuses on. `tip` is the entry's tip, for a problem
/// with one entry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Problem {
    pub tip: Option<String>,
    pub message: String,
}

impl Problem {
    pub fn of(tip: &str, message: impl Into<String>) -> Self {
        Self {
            tip: Some(tip.to_string()),
            message: message.into(),
        }
    }
}

/// Resolve a tip to the one visible change it names: a local bookmark, or a
/// change ID or a unique prefix of one. The error says why it can't be used.
pub fn resolve_tip(jj: &Jujutsu, tip: &str) -> std::result::Result<Oid, String> {
    if tip.starts_with('@') {
        return Err(format!(
            "The tip '{tip}' is relative to the working copy, so it moves. Use a \
             local bookmark or a change ID."
        ));
    }
    if tip.contains('@') {
        return Err(format!(
            "The tip '{tip}' is a remote bookmark. Use a local bookmark or a change ID."
        ));
    }
    let jj_error = |e: Error| format!("The tip '{tip}' could not be resolved: {e}");
    let quoted = format!("\"{}\"", tip.replace('\\', "\\\\").replace('"', "\\\""));
    match jj
        .commit_ids(&format!("bookmarks(exact:{quoted})"))
        .map_err(jj_error)?[..]
    {
        [commit] => return Ok(commit),
        [] => {}
        ref commits => {
            return Err(format!(
                "The bookmark '{tip}' is conflicted: it points to {} changes. Resolve it \
                 with `jj bookmark set`.",
                commits.len()
            ));
        }
    }
    if tip.is_empty() || !tip.chars().all(|c| ('k'..='z').contains(&c)) {
        return Err(format!(
            "The tip '{tip}' is not a local bookmark or a change ID."
        ));
    }
    match jj
        .commit_ids(&format!("change_id({tip})"))
        .map_err(jj_error)?[..]
    {
        [commit] => Ok(commit),
        [] => Err(format!(
            "The tip '{tip}' is not a local bookmark or a visible change. It may be \
             missing or hidden (abandoned, or rewritten under a new change ID)."
        )),
        ref commits => Err(format!(
            "The tip '{tip}' is divergent: its change ID has {} visible commits. Keep \
             one (abandon the others with `jj abandon <commit ID>`).",
            commits.len()
        )),
    }
}

/// One change of an entry.
#[derive(Debug, Clone)]
pub struct Member {
    pub change_id: String,
    /// The change on its own (its `lower` is empty).
    pub commit: PreparedCommit,
}

impl Member {
    pub fn title(&self) -> &str {
        self.commit
            .message
            .get(&MessageSection::Title)
            .map(String::as_str)
            .unwrap_or("")
    }
}

/// One entry resolved against the repository.
#[derive(Debug, Clone)]
pub struct Resolved {
    /// The entry's index in the file.
    pub index: usize,
    /// The top change, when the tip resolves.
    pub tip: Option<Oid>,
    /// The entry's changes, bottom first. Empty when they could not be
    /// worked out.
    pub members: Vec<Member>,
}

impl Resolved {
    /// The entry as one planned Pull Request (see `PreparedCommit::lower`)
    /// with Pull Request number `pr`. The descriptions keep only their title
    /// and summary: the stack file has the rest.
    pub fn planned(&self, pr: Option<u64>) -> PreparedCommit {
        let mut lower: Vec<PreparedCommit> = self
            .members
            .iter()
            .map(|m| {
                let mut commit = m.commit.clone();
                commit.message.retain(|section, _| {
                    matches!(section, MessageSection::Title | MessageSection::Summary)
                });
                commit.pull_request_number = pr;
                commit
            })
            .collect();
        let mut top = lower.pop().expect("an entry has a change");
        if let Some(bottom) = lower.first() {
            top.parent_oid = bottom.parent_oid;
        }
        top.lower = lower;
        top
    }
}

/// Resolve the entries at `indices` (bottom first) and their changes, and
/// find every problem with them that does not need GitHub: tips that don't
/// resolve or are not in a line, entries without changes, and changes that
/// are conflicted, have no description, or have a `Pull Request:` line.
pub fn resolve(
    jj: &Jujutsu,
    config: &Config,
    file: &StackFile,
    indices: &[usize],
) -> Result<(Vec<Resolved>, Vec<Problem>)> {
    let mut problems = Vec::new();
    let mut resolved = Vec::new();
    // The tip below the next entry: `Some(None)` for trunk, `None` when it
    // is not known.
    let mut below: Option<Option<(Oid, &str)>> = Some(None);
    for &index in indices {
        let tip = file.prs[index].tip.as_str();
        let mut entry = Resolved {
            index,
            tip: None,
            members: Vec::new(),
        };
        match resolve_tip(jj, tip) {
            Ok(commit) => entry.tip = Some(commit),
            Err(message) => problems.push(Problem::of(tip, message)),
        }
        let next_below = entry.tip.map(|commit| Some((commit, tip)));
        if let (Some(commit), Some(below)) = (entry.tip, below) {
            match members(jj, config, tip, commit, below)? {
                Ok(members) => {
                    problems.extend(check_members(jj, tip, &members)?);
                    entry.members = members;
                }
                Err(problem) => problems.push(problem),
            }
        }
        // An entry whose tip is not above the one below still has a known
        // tip for the entry above it.
        below = next_below;
        resolved.push(entry);
    }
    Ok((resolved, problems))
}

/// The changes of the entry with tip `tip` (at `commit`), bottom first: from
/// it down to `below` (another entry's tip, or trunk when `None`). They have
/// to be a line of changes, each with one parent.
fn members(
    jj: &Jujutsu,
    config: &Config,
    tip: &str,
    commit: Oid,
    below: Option<(Oid, &str)>,
) -> Result<std::result::Result<Vec<Member>, Problem>> {
    let trunk = config.trunk_revset();
    let below_revset = match below {
        Some((below, below_tip)) => {
            if below == commit {
                return Ok(Err(Problem::of(
                    tip,
                    format!(
                        "'{tip}' and the entry below it, '{below_tip}', point to the same change."
                    ),
                )));
            }
            if !jj.git_repo.graph_descendant_of(commit, below)? {
                return Ok(Err(Problem::of(
                    tip,
                    format!(
                        "The tips are not in a line: '{tip}' is not above '{below_tip}', the \
                         tip of the entry below it."
                    ),
                )));
            }
            below.to_string()
        }
        None => trunk.clone(),
    };
    let mut oids = jj.commit_ids(&format!("({below_revset})..{commit}"))?;
    oids.reverse();
    if oids.is_empty() {
        return Ok(Err(Problem::of(
            tip,
            format!("'{tip}' has no changes: it is already in {trunk}."),
        )));
    }
    let commits: Vec<PreparedCommit> = oids
        .iter()
        .map(|&oid| jj.prepare_commit(config, oid))
        .collect::<Result<_>>()?;
    let change_ids = jj.change_ids_for(&commits)?;
    let members: Vec<Member> = commits
        .into_iter()
        .map(|commit| Member {
            change_id: change_ids.get(&commit.oid).cloned().unwrap_or_default(),
            commit,
        })
        .collect();
    for (i, member) in members.iter().enumerate() {
        let parents = jj.git_repo.find_commit(member.commit.oid)?.parent_count();
        let follows = i == 0 || member.commit.parent_oid == members[i - 1].commit.oid;
        if parents != 1 || !follows {
            let what = if parents != 1 {
                "is a merge"
            } else {
                "is on a separate branch"
            };
            return Ok(Err(Problem::of(
                tip,
                format!(
                    "The changes of '{tip}' are not in a line: {} {what}.",
                    short(&member.change_id)
                ),
            )));
        }
    }
    Ok(Ok(members))
}

/// Problems with an entry's changes: conflicts, no description, or a
/// `Pull Request:` line.
fn check_members(jj: &Jujutsu, tip: &str, members: &[Member]) -> Result<Vec<Problem>> {
    let mut problems = Vec::new();
    let revset = members
        .iter()
        .map(|m| m.commit.oid.to_string())
        .collect::<Vec<_>>()
        .join(" | ");
    let conflicted = jj.conflicted_changes(&revset)?;
    if !conflicted.is_empty() {
        problems.push(Problem::of(
            tip,
            format!(
                "These changes of '{tip}' have conflicts:\n{conflicted}\nResolve them (for \
                 example with `jj resolve`)."
            ),
        ));
    }
    let ids = |members: &[&Member]| {
        members
            .iter()
            .map(|m| short(&m.change_id))
            .collect::<Vec<_>>()
            .join(", ")
    };
    let undescribed: Vec<&Member> = members
        .iter()
        .filter(|m| m.title().trim().is_empty())
        .collect();
    if !undescribed.is_empty() {
        problems.push(Problem::of(
            tip,
            format!(
                "These changes of '{tip}' have no description: {}. Each change becomes a \
                 commit on the Pull Request's branch. Describe them with `jj describe`.",
                ids(&undescribed)
            ),
        ));
    }
    let linked: Vec<&Member> = members
        .iter()
        .filter(|m| {
            m.commit
                .message
                .get(&MessageSection::PullRequest)
                .is_some_and(|line| !line.trim().is_empty())
        })
        .collect();
    if !linked.is_empty() {
        problems.push(Problem::of(
            tip,
            format!(
                "These changes of '{tip}' have a `Pull Request:` line: {}. A change is \
                 submitted with a stack file or with `jj spr diff`, not both. Remove the \
                 line with `jj describe`, and close the Pull Request it links if it is no \
                 longer needed.",
                ids(&linked)
            ),
        ));
    }
    Ok(problems)
}

/// One entry checked against the repository and GitHub.
#[derive(Debug, Clone)]
pub struct Checked {
    pub resolved: Resolved,
    /// The entry's title and body, when they can be read.
    pub text: Option<(Text, TitleSource)>,
    pub pull_request: Option<PullRequest>,
    /// The title and body its Pull Request should have after `submit`: the
    /// file's, or GitHub's when only GitHub's changed since the last push.
    /// `None` when they can't be read or both changed.
    pub wanted: Option<Text>,
}

impl Checked {
    /// Whether `wanted` is the file's text, so `submit` records it in
    /// `synced`.
    pub fn wants_file_text(&self) -> bool {
        matches!((&self.wanted, &self.text), (Some(wanted), Some((text, _))) if wanted == text)
    }

    /// What `submit` keeps of GitHub's text in place of the file's, because
    /// only GitHub's changed since the last submit: "title", "body", or
    /// "title and body".
    pub fn kept_from_github(&self) -> Option<&'static str> {
        let (wanted, (text, _)) = (self.wanted.as_ref()?, self.text.as_ref()?);
        match (wanted.title != text.title, wanted.body != text.body) {
            (true, true) => Some("title and body"),
            (true, false) => Some("title"),
            (false, true) => Some("body"),
            (false, false) => None,
        }
    }
}

/// A stack file checked against the repository and GitHub: its entries
/// at the indices given to `check`, and every problem `submit` refuses on.
#[derive(Debug, Clone)]
pub struct Check {
    pub entries: Vec<Checked>,
    pub problems: Vec<Problem>,
}

/// Check the entries at `indices` (bottom first) for everything `submit`
/// refuses on: the problems `resolve` finds, entries without a title,
/// Pull Requests that merged or were closed, open Pull Requests in their
/// GitHub stack that the file doesn't list, entries whose Pull Requests
/// are in another order on GitHub, and titles and bodies that changed both in
/// the file and on GitHub since the last push (unless `overwrite_github`).
/// `dir` is the stack file's directory, and `name` its path, for messages.
#[allow(clippy::too_many_arguments)]
pub async fn check(
    jj: &Jujutsu,
    gh: &GitHub,
    config: &Config,
    file: &StackFile,
    dir: &Path,
    name: &str,
    indices: &[usize],
    overwrite_github: bool,
) -> Result<Check> {
    let (resolved, mut problems) = resolve(jj, config, file, indices)?;
    let pull_requests = gh
        .get_pull_requests(indices.iter().map(|&i| file.prs[i].pr))
        .await?;

    let mut entries = Vec::new();
    for (resolved, pull_request) in resolved.into_iter().zip(pull_requests) {
        let entry = &file.prs[resolved.index];
        let tip = entry.tip.as_str();
        let text = match entry.text(dir) {
            Ok(text) => Some(text),
            Err(message) => {
                problems.push(Problem::of(tip, message));
                None
            }
        };
        if let Some(pr) = &pull_request {
            let number = pr.number;
            match pr.state {
                PullRequestState::Open => {}
                PullRequestState::Merged => problems.push(Problem::of(
                    tip,
                    format!(
                        "The Pull Request of '{tip}', #{number}, has merged. Run `jj spr \
                         sync {name}` to abandon its changes and mark the entry merged."
                    ),
                )),
                PullRequestState::Closed => problems.push(Problem::of(
                    tip,
                    format!(
                        "The Pull Request of '{tip}', #{number}, is closed. Remove the entry, \
                         or remove its pr and synced keys to open a new Pull Request."
                    ),
                )),
            }
        }
        let wanted = match (&text, &pull_request) {
            (Some((text, _)), None) => Some(text.clone()),
            (Some((text, _)), Some(pr)) => match wanted_text(entry, text, pr) {
                Wanted::Text(wanted) => Some(wanted),
                Wanted::BothChanged if overwrite_github => Some(text.clone()),
                Wanted::BothChanged => {
                    problems.push(Problem::of(
                        tip,
                        format!(
                            "The title or body of '{tip}' changed both in the stack file and on \
                             GitHub (#{}) since the last submit. Copy GitHub's edits into the \
                             file, or pass --overwrite-github to replace them.",
                            pr.number
                        ),
                    ));
                    None
                }
            },
            (None, _) => None,
        };
        entries.push(Checked {
            resolved,
            text,
            pull_request,
            wanted,
        });
    }

    problems.extend(check_stack(gh, file, name, &entries).await?);
    Ok(Check { entries, problems })
}

enum Wanted {
    Text(Text),
    BothChanged,
}

/// The title and body an existing Pull Request should have: the file's when
/// they changed since the last push (or match GitHub's), GitHub's when only
/// those changed. Without `synced`, both count as changed.
fn wanted_text(entry: &Entry, text: &Text, pr: &PullRequest) -> Wanted {
    let github = Text::new(&pr.title, pr.body.as_deref().unwrap_or_default());
    let synced = entry.synced.as_deref();
    let file_changed = synced != Some(&text.hash());
    let github_changed = synced != Some(&github.hash());
    if github == *text || !github_changed {
        Wanted::Text(text.clone())
    } else if !file_changed {
        Wanted::Text(github)
    } else {
        Wanted::BothChanged
    }
}

/// Open Pull Requests in the GitHub stacks of the entries that the file
/// doesn't list.
async fn check_stack(
    gh: &GitHub,
    file: &StackFile,
    name: &str,
    entries: &[Checked],
) -> Result<Vec<Problem>> {
    let open: Vec<u64> = entries
        .iter()
        .filter_map(|e| e.pull_request.as_ref())
        .filter(|pr| pr.state == PullRequestState::Open)
        .map(|pr| pr.number)
        .collect();
    if open.is_empty() {
        return Ok(Vec::new());
    }
    let listed: Vec<u64> = file.prs.iter().filter_map(|e| e.pr).collect();
    let mut problems = Vec::new();
    for stack in gh.list_stacks().await?.unwrap_or_default() {
        let active = stack.active_pull_requests();
        if !active.iter().any(|n| open.contains(n)) {
            continue;
        }
        let unlisted: Vec<u64> = active
            .iter()
            .copied()
            .filter(|n| !listed.contains(n))
            .collect();
        if !unlisted.is_empty() {
            problems.push(Problem {
                tip: None,
                message: format!(
                    "GitHub stack #{} has {}, which {} not in {name}. Put the entry back \
                     and close it with `jj spr close {name} --pr <tip>`, or close it on \
                     GitHub.",
                    stack.number,
                    format_pr_list(&unlisted),
                    if unlisted.len() == 1 { "is" } else { "are" },
                ),
            });
        }
    }
    Ok(problems)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn writes_keys_in_a_fixed_order_and_keeps_unknown_ones() {
        let text = r#"{"zeta": 1, "prs": [
            {"merged": true, "x-note": "kept", "pushed": "c0ffee", "synced": "sha256:00",
             "pr": 51, "body": "a.md", "title": "A", "tip": "a"},
            {"tip": "b"}
        ]}"#;
        let file = StackFile::parse(text).unwrap();
        assert_eq!(
            file.to_text(),
            r#"{
  "prs": [
    {
      "tip": "a",
      "title": "A",
      "body": "a.md",
      "pr": 51,
      "synced": "sha256:00",
      "pushed": "c0ffee",
      "merged": true,
      "x-note": "kept"
    },
    {
      "tip": "b"
    }
  ],
  "zeta": 1
}
"#
        );
        // Reading what was written gives the same text.
        assert_eq!(
            StackFile::parse(&file.to_text()).unwrap().to_text(),
            file.to_text()
        );
    }

    #[test]
    fn rejects_entries_without_a_tip() {
        assert!(StackFile::parse(r#"{"prs": [{"title": "A"}]}"#).is_err());
        assert!(StackFile::parse(r#"{"prs": [{"tip": "a", "pr": "51"}]}"#).is_err());
        assert!(StackFile::parse(r#"{}"#).is_err());
    }

    #[test]
    fn active_entries_skip_merged_ones_and_stop_at_upto() {
        let file = StackFile::parse(
            r#"{"prs": [{"tip": "a", "merged": true}, {"tip": "b"}, {"tip": "c"}]}"#,
        )
        .unwrap();
        assert_eq!(file.active(None).unwrap(), vec![1, 2]);
        assert_eq!(file.active(Some("b")).unwrap(), vec![1]);
        assert!(file.active(Some("a")).is_err());
        assert!(file.active(Some("d")).is_err());
    }

    #[test]
    fn heading_is_the_title() {
        assert_eq!(
            split_heading("# Title\n\nBody.\n"),
            (Some("Title"), "\nBody.\n")
        );
        assert_eq!(split_heading("\n# Title"), (Some("Title"), ""));
        assert_eq!(
            split_heading("## Not a title\n"),
            (None, "## Not a title\n")
        );
        assert_eq!(split_heading("Body only"), (None, "Body only"));
        assert_eq!(split_heading("#\nBody"), (None, "#\nBody"));
    }

    fn entry(title: Option<&str>, body: Option<&str>) -> Entry {
        Entry {
            tip: "a".into(),
            title: title.map(Into::into),
            body: body.map(Into::into),
            ..Default::default()
        }
    }

    #[test]
    fn text_from_the_body_file_or_the_title_key() {
        let dir = tempfile::TempDir::new().unwrap();
        std::fs::write(dir.path().join("h.md"), "# From heading\n\nThe body.\n").unwrap();
        std::fs::write(dir.path().join("plain.md"), "Just a body.\r\nTwo lines.\n").unwrap();

        let (text, source) = entry(Some("Key"), Some("h.md")).text(dir.path()).unwrap();
        assert_eq!(text, Text::new("From heading", "The body."));
        assert_eq!(source, TitleSource::Heading);

        let (text, source) = entry(Some("Key"), Some("plain.md"))
            .text(dir.path())
            .unwrap();
        assert_eq!(text.title, "Key");
        assert_eq!(text.body, "Just a body.\nTwo lines.");
        assert_eq!(source, TitleSource::Key);

        let (text, _) = entry(Some("Key"), None).text(dir.path()).unwrap();
        assert_eq!(text, Text::new("Key", ""));

        assert!(entry(None, Some("plain.md")).text(dir.path()).is_err());
        assert!(entry(Some("  "), None).text(dir.path()).is_err());
        let missing = entry(Some("Key"), Some("missing.md"))
            .text(dir.path())
            .unwrap_err();
        assert!(missing.contains("missing.md"), "{missing}");
    }

    #[test]
    fn normalized_text_hashes_alike() {
        let a = Text::new(" Title ", "\n\nBody\r\nmore\n\n");
        let b = Text::new("Title", "Body\nmore");
        assert_eq!(a, b);
        assert_eq!(a.hash(), b.hash());
        assert!(a.hash().starts_with("sha256:"));
        assert_ne!(
            Text::new("Title", "Body").hash(),
            Text::new("Title\nBody", "").hash()
        );
    }

    #[test]
    fn writes_and_creates() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("stack").join("stack.json");
        let file = StackFile {
            prs: vec![entry(Some("A"), None)],
            ..Default::default()
        };
        file.create(&path).unwrap();
        assert!(file.create(&path).is_err(), "create refuses to replace");
        let mut read = StackFile::read(&path).unwrap();
        read.prs[0].pr = Some(7);
        read.write(&path).unwrap();
        assert_eq!(StackFile::read(&path).unwrap().prs[0].pr, Some(7));
        assert_eq!(base_dir(&path), dir.path().join("stack"));
        assert_eq!(base_dir(Path::new("stack.json")), PathBuf::from("."));
    }
}
