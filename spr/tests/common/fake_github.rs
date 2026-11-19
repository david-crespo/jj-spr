//! A fake GitHub API for testing jj-spr's native stack support.
//!
//! It serves the REST and GraphQL calls jj-spr makes, backed by a bare Git
//! repository that jj-spr pushes to, and models GitHub's stacked pull requests.
//!
//! The stack behaviour follows the fake GitHub in jj-stack
//! (<https://github.com/bos/jj-stack>, `tests/support/fake_github.py`,
//! Apache-2.0), whose authors checked it against the real API:
//!
//! - a stack needs two or more pull requests, each PR's base must be the head
//!   branch of the PR below it, and a PR can be in only one stack
//! - the base of a stacked PR cannot be changed with PATCH (422)
//! - stacked PRs are merged with `PUT .../merge-async` and polled with
//!   `GET .../merge-async/{uuid}`; merging a PR merges the stack prefix below
//!   it, and GitHub then rebases the remaining members onto the new base
//! - merged members stay listed in the stack as history
//!
//! Checked against real GitHub with `tests/live/run_live_github.sh`:
//!
//! - stacks are numbered from the same sequence as pull requests and issues
//! - the ordinary merge endpoint refuses stacked PRs (403), and so does
//!   "update branch" (403)
//! - after a stacked merge, GitHub rewrites the branches of the remaining
//!   members: it replays each of their commits onto the new base, keeping
//!   message and author, and drops merge commits and commits that the new
//!   base makes empty. A member with no commits left to replay is reset to
//!   its new base's head and closed.
//! - when replaying a member's commits would conflict, GitHub rewrites no
//!   branch at all. It still points the Pull Request above the merged ones
//!   at the stack's base, and reports that Pull Request as `CONFLICTING`.
//! - after a stacked merge, GitHub points the next PR at the stack's base a
//!   few seconds before it pushes the rebased head. In between, it can
//!   report whether the old head can be merged (`stale_head_polls`).
//! - a draft cannot be merged: 405 from the ordinary endpoint, and a failed
//!   merge (400) from `merge-async`
//! - a PR in the middle of a stack can be closed; the stack still lists it
//! - a PR whose head becomes part of its base branch (as when a push to the
//!   base branch brings in the PR's head) is marked merged
//!
//! Everything else (closing PRs when their base or head branch is deleted,
//! squash merges as a three-way merge, "update branch" merging the base into
//! an ordinary PR) is ordinary GitHub behaviour.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::JoinHandle;

use axum::body::Bytes;
use axum::http::{Method, StatusCode, Uri, header};
use axum::response::{IntoResponse, Response};

use serde_json::{Value, json};

const GIT_IDENTITY: [(&str, &str); 4] = [
    ("GIT_AUTHOR_NAME", "Fake GitHub"),
    ("GIT_AUTHOR_EMAIL", "fake-github@example.com"),
    ("GIT_COMMITTER_NAME", "Fake GitHub"),
    ("GIT_COMMITTER_EMAIL", "fake-github@example.com"),
];

#[derive(Debug, Clone)]
pub struct PullRequest {
    pub number: u64,
    pub title: String,
    pub body: String,
    pub head_ref: String,
    pub base_ref: String,
    pub draft: bool,
    pub state: String,
    pub merged_at: Option<String>,
    pub merge_commit_sha: Option<String>,
    pub last_head_sha: String,
    pub closed_reason: Option<String>,
    /// Latest review per reviewer: (login, "APPROVED" | "CHANGES_REQUESTED").
    pub reviews: Vec<(String, String)>,
    pub threads: Vec<ReviewThread>,
}

#[derive(Debug, Clone)]
pub struct ReviewThread {
    pub path: String,
    pub line: u64,
    pub author: String,
    pub body: String,
    pub resolved: bool,
}

impl PullRequest {
    pub fn is_open(&self) -> bool {
        self.state == "open"
    }
}

#[derive(Debug, Clone)]
struct MergeOperation {
    uuid: String,
    pr_number: u64,
    method: String,
    expected_head: String,
    /// `commit_title` and `commit_message`, which GitHub applies only to
    /// this Pull Request's commit, not to the ones merged below it.
    commit: Option<(String, String)>,
    status: String,
    message: String,
    sha: Option<String>,
}

/// One recorded API request: method, path and JSON body.
#[derive(Debug, Clone)]
pub struct Recorded {
    pub method: String,
    pub path: String,
    pub body: Value,
}

/// State of the fake repository.
#[derive(Debug)]
pub struct State {
    pub owner: String,
    pub name: String,
    pub git_dir: PathBuf,
    pub stacks_enabled: bool,
    pub prs: BTreeMap<u64, PullRequest>,
    pub stacks: BTreeMap<u64, Vec<u64>>,
    /// CI checks by commit: (name, "success" | "failure" | "pending"). Like
    /// GitHub's, they belong to a head commit, so pushing clears them.
    pub checks: HashMap<String, Vec<(String, String)>>,
    merge_ops: HashMap<u64, MergeOperation>,
    /// GraphQL operations (by name) that answer with an error.
    pub failing_queries: Vec<String>,
    /// How many more mergeability queries answer `UNKNOWN`, as GitHub does
    /// while it is still computing mergeability.
    pub unknown_mergeability_polls: u32,
    /// After the next stack merge, how many mergeability queries see the
    /// Pull Request above the merged ones already targeting the stack's base
    /// but still at its old head, before the rebase of the rest of the stack
    /// lands. Live GitHub leaves a gap of 2 to 7 seconds between the two.
    pub stale_head_polls: u32,
    /// The rebase of the rest of the stack that `stale_head_polls` holds
    /// back: the members, the stack's base, and the old head of the top
    /// merged one.
    held_rebase: Option<(Vec<u64>, String, String)>,
    /// Pull Requests whose merge GitHub blocks (for required checks or
    /// reviews) although they have no conflicts.
    pub blocked: Vec<u64>,
    /// Head branches whose next Pull Request GitHub refuses to create, once.
    pub failing_creations: Vec<String>,
    pub requests: Vec<Recorded>,
    /// Pull requests and stacks share one sequence of numbers, as on GitHub.
    next_number: u64,
}

struct ApiError {
    status: u16,
    message: String,
    /// The `errors` of a 422 "Validation Failed" reply.
    errors: Vec<Value>,
}

fn api_error<T>(status: u16, message: impl Into<String>) -> Result<T, ApiError> {
    Err(ApiError {
        status,
        message: message.into(),
        errors: Vec::new(),
    })
}

type Reply = Result<(u16, Option<Value>), ApiError>;

fn now() -> String {
    // A fixed-format timestamp is all the clients look at.
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let days = secs / 86_400;
    let (y, m, d) = civil_from_days(days as i64);
    let rem = secs % 86_400;
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z",
        rem / 3600,
        (rem / 60) % 60,
        rem % 60
    )
}

/// Days since the Unix epoch to a (year, month, day) civil date.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = yoe + era * 400 + if m <= 2 { 1 } else { 0 };
    (y, m, d)
}

impl State {
    // -- git ---------------------------------------------------------------

    pub fn git(&self, args: &[&str]) -> String {
        self.run_git(args, &[])
    }

    /// Like `git`, but with the author taken from `commit`.
    fn git_as_author_of(&self, commit: &str, args: &[&str]) -> String {
        let author = self.git(&["log", "-1", "--format=%an%x00%ae%x00%aI", commit]);
        let mut fields = author.split('\0');
        let mut envs = Vec::new();
        for var in ["GIT_AUTHOR_NAME", "GIT_AUTHOR_EMAIL", "GIT_AUTHOR_DATE"] {
            envs.push((var, fields.next().unwrap_or_default().to_string()));
        }
        self.run_git(args, &envs)
    }

    fn run_git(&self, args: &[&str], envs: &[(&str, String)]) -> String {
        let output = Command::new("git")
            .arg("--git-dir")
            .arg(&self.git_dir)
            .args(args)
            .envs(GIT_IDENTITY)
            .envs(envs.iter().map(|(k, v)| (k, v)))
            .stdin(Stdio::null())
            .output()
            .expect("failed to run git");
        assert!(
            output.status.success(),
            "fake github: git {args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).trim().to_string()
    }

    /// `merge-tree --write-tree`, optionally with a merge base: the merged
    /// tree, or `None` when the merge conflicts.
    fn merged_tree(&self, merge_base: Option<&str>, ours: &str, theirs: &str) -> Option<String> {
        let mut command = Command::new("git");
        command
            .arg("--git-dir")
            .arg(&self.git_dir)
            .args(["merge-tree", "--write-tree"]);
        if let Some(merge_base) = merge_base {
            command.arg(format!("--merge-base={merge_base}"));
        }
        let output = command
            .args([ours, theirs])
            .stdin(Stdio::null())
            .output()
            .expect("failed to run git");
        match output.status.code() {
            Some(0) => {
                let tree = String::from_utf8_lossy(&output.stdout);
                Some(tree.lines().next().unwrap().to_string())
            }
            Some(1) => None,
            _ => panic!(
                "fake github: git merge-tree {ours} {theirs} failed: {}",
                String::from_utf8_lossy(&output.stderr)
            ),
        }
    }

    /// Commit `tree` onto `parents`, with the author of `author_of` if given.
    fn commit_tree(
        &self,
        tree: &str,
        parents: &[&str],
        message: &str,
        author_of: Option<&str>,
    ) -> String {
        let mut commit = vec!["commit-tree", tree];
        for parent in parents {
            commit.extend(["-p", parent]);
        }
        commit.extend(["-m", message]);
        match author_of {
            Some(original) => self.git_as_author_of(original, &commit),
            None => self.git(&commit),
        }
    }

    fn tree_of(&self, commit: &str) -> String {
        self.git(&["rev-parse", &format!("{commit}^{{tree}}")])
    }

    pub fn branches(&self) -> BTreeMap<String, String> {
        let output = Command::new("git")
            .arg("--git-dir")
            .arg(&self.git_dir)
            .args(["show-ref", "--heads"])
            .output()
            .expect("failed to run git");
        String::from_utf8_lossy(&output.stdout)
            .lines()
            .filter_map(|line| {
                let (sha, name) = line.split_once(' ')?;
                Some((
                    name.strip_prefix("refs/heads/")?.to_string(),
                    sha.to_string(),
                ))
            })
            .collect()
    }

    pub fn is_ancestor(&self, ancestor: &str, descendant: &str) -> bool {
        Command::new("git")
            .arg("--git-dir")
            .arg(&self.git_dir)
            .args(["merge-base", "--is-ancestor", ancestor, descendant])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    }

    // -- pull request state ------------------------------------------------

    /// Apply what GitHub does on its own when branches change.
    fn refresh(&mut self) {
        let heads = self.branches();
        // GitHub marks a Pull Request merged once its head is in its base
        // branch, however it got there.
        let landed: Vec<u64> = self
            .prs
            .values()
            .filter(|pr| pr.is_open())
            .filter(
                |pr| match (heads.get(&pr.head_ref), heads.get(&pr.base_ref)) {
                    (Some(head), Some(base)) => self.is_ancestor(head, base),
                    _ => false,
                },
            )
            .map(|pr| pr.number)
            .collect();
        for pr in self.prs.values_mut() {
            if !pr.is_open() {
                continue;
            }
            if let Some(sha) = heads.get(&pr.head_ref) {
                pr.last_head_sha = sha.clone();
            }
            if !heads.contains_key(&pr.base_ref) {
                pr.state = "closed".into();
                pr.closed_reason = Some(format!("base branch {} was deleted", pr.base_ref));
            } else if !heads.contains_key(&pr.head_ref) {
                pr.state = "closed".into();
                pr.closed_reason = Some(format!("head branch {} was deleted", pr.head_ref));
            } else if landed.contains(&pr.number) {
                pr.state = "closed".into();
                pr.merged_at = Some(now());
                pr.merge_commit_sha = Some(pr.last_head_sha.clone());
                pr.closed_reason = Some(format!("its head is in {}", pr.base_ref));
            }
        }
    }

    pub fn stack_of(&self, number: u64) -> Option<u64> {
        self.stacks
            .iter()
            .find(|(_, members)| members.contains(&number))
            .map(|(n, _)| *n)
    }

    pub fn active(&self, members: &[u64]) -> Vec<u64> {
        members
            .iter()
            .copied()
            .filter(|n| self.prs[n].is_open())
            .collect()
    }

    fn pr_mut(&mut self, number: u64) -> Result<&mut PullRequest, ApiError> {
        match self.prs.get_mut(&number) {
            Some(pr) => Ok(pr),
            None => api_error(404, "Not Found"),
        }
    }

    // -- payloads ----------------------------------------------------------

    fn web(&self, path: &str) -> String {
        format!("https://github.com/{}/{}{}", self.owner, self.name, path)
    }

    fn api(&self, path: &str) -> String {
        format!(
            "https://api.github.test/repos/{}/{}{}",
            self.owner, self.name, path
        )
    }

    fn user_payload() -> Value {
        let base = "https://api.github.test/users/fake";
        json!({
            "login": "fake", "id": 1, "node_id": "U_1",
            "avatar_url": "https://avatars.github.test/u/1", "gravatar_id": "",
            "url": base, "html_url": "https://github.com/fake",
            "followers_url": format!("{base}/followers"),
            "following_url": format!("{base}/following"),
            "gists_url": format!("{base}/gists"), "starred_url": format!("{base}/starred"),
            "subscriptions_url": format!("{base}/subscriptions"),
            "organizations_url": format!("{base}/orgs"), "repos_url": format!("{base}/repos"),
            "events_url": format!("{base}/events"),
            "received_events_url": format!("{base}/received_events"),
            "type": "User", "site_admin": false,
        })
    }

    fn pr_payload(&self, number: u64) -> Value {
        let pr = &self.prs[&number];
        let heads = self.branches();
        let n = pr.number;
        let head_sha = heads.get(&pr.head_ref).unwrap_or(&pr.last_head_sha).clone();
        json!({
            "url": self.api(&format!("/pulls/{n}")), "id": 1000 + n, "node_id": format!("PR_{n}"),
            "html_url": self.web(&format!("/pull/{n}")),
            "diff_url": self.web(&format!("/pull/{n}.diff")),
            "patch_url": self.web(&format!("/pull/{n}.patch")),
            "issue_url": self.api(&format!("/issues/{n}")),
            "commits_url": self.api(&format!("/pulls/{n}/commits")),
            "review_comments_url": self.api(&format!("/pulls/{n}/comments")),
            "review_comment_url": self.api("/pulls/comments{/number}"),
            "comments_url": self.api(&format!("/issues/{n}/comments")),
            "statuses_url": self.api(&format!("/statuses/{head_sha}")),
            "number": n, "state": pr.state, "locked": false, "title": pr.title,
            "user": Self::user_payload(), "body": pr.body, "labels": [],
            "created_at": "2026-01-01T00:00:00Z", "updated_at": now(),
            "closed_at": if pr.is_open() { Value::Null } else { json!(now()) },
            "merged": pr.merged_at.is_some(), "merged_at": pr.merged_at,
            "merge_commit_sha": pr.merge_commit_sha,
            "assignees": [], "requested_reviewers": [], "requested_teams": [],
            "head": {"label": format!("{}:{}", self.owner, pr.head_ref), "ref": pr.head_ref,
                     "sha": head_sha},
            "base": {"label": format!("{}:{}", self.owner, pr.base_ref), "ref": pr.base_ref,
                     "sha": heads.get(&pr.base_ref).cloned().unwrap_or_default()},
            "_links": {}, "author_association": "OWNER", "draft": pr.draft,
            "additions": 0, "deletions": 0, "changed_files": 0, "commits": 1,
            "review_comments": 0, "comments": 0,
        })
    }

    fn stack_payload(&self, number: u64) -> Value {
        let heads = self.branches();
        let members: Vec<Value> = self.stacks[&number]
            .iter()
            .map(|n| {
                let pr = &self.prs[n];
                json!({
                    "number": n,
                    "state": pr.state,
                    "draft": pr.draft,
                    "merged_at": pr.merged_at,
                    "head": {"ref": pr.head_ref,
                             "sha": heads.get(&pr.head_ref).unwrap_or(&pr.last_head_sha)},
                })
            })
            .collect();
        let base = self.stacks[&number]
            .first()
            .map(|n| self.prs[n].base_ref.clone())
            .unwrap_or_default();
        let open = self.stacks[&number].iter().any(|n| self.prs[n].is_open());
        json!({"id": 1_000_000 + number, "number": number, "base": {"ref": base},
               "open": open, "pull_requests": members})
    }

    fn graphql_pr(&self, number: u64) -> Value {
        let pr = &self.prs[&number];
        let state = if pr.merged_at.is_some() {
            "MERGED".to_string()
        } else {
            pr.state.to_uppercase()
        };
        let has = |wanted: &str| pr.reviews.iter().any(|(_, state)| state == wanted);
        let decision = if has("CHANGES_REQUESTED") {
            json!("CHANGES_REQUESTED")
        } else if has("APPROVED") {
            json!("APPROVED")
        } else {
            Value::Null
        };
        let reviews: Vec<Value> = pr
            .reviews
            .iter()
            .map(|(login, state)| {
                json!({"author": {"__typename": "User", "login": login}, "state": state})
            })
            .collect();
        let conflicting = self.is_conflicting(number);
        json!({
            "number": pr.number,
            "state": state,
            "reviewDecision": decision,
            "isDraft": pr.draft,
            "title": pr.title,
            "body": pr.body,
            "baseRefName": pr.base_ref,
            "headRefName": pr.head_ref,
            "headRefOid": self.branches().get(&pr.head_ref).unwrap_or(&pr.last_head_sha),
            "mergeable": if conflicting { "CONFLICTING" } else { "MERGEABLE" },
            "mergeStateStatus": if conflicting {
                "DIRTY"
            } else if self.blocked.contains(&number) {
                "BLOCKED"
            } else {
                "CLEAN"
            },
            "mergeCommit": pr.merge_commit_sha.as_ref().map(|oid| json!({"oid": oid})),
            "latestOpinionatedReviews": {"nodes": reviews},
            "reviewRequests": {"nodes": []},
        })
    }

    /// Whether merging the Pull Request's head into its base conflicts.
    fn is_conflicting(&self, number: u64) -> bool {
        let pr = &self.prs[&number];
        let heads = self.branches();
        match (heads.get(&pr.base_ref), heads.get(&pr.head_ref)) {
            (Some(base), Some(head)) if pr.is_open() => {
                self.merged_tree(None, base, head).is_none()
            }
            _ => false,
        }
    }

    fn graphql_pr_status(&self, number: u64) -> Value {
        let pr = &self.prs[&number];
        let head = self
            .branches()
            .get(&pr.head_ref)
            .cloned()
            .unwrap_or_else(|| pr.last_head_sha.clone());
        let rollup = self
            .checks
            .get(&head)
            .filter(|c| !c.is_empty())
            .map(|checks| {
                let nodes: Vec<Value> = checks
                    .iter()
                    .map(|(name, state)| {
                        let (status, conclusion) = match state.as_str() {
                            "pending" => ("IN_PROGRESS", Value::Null),
                            other => ("COMPLETED", json!(other.to_uppercase())),
                        };
                        json!({"__typename": "CheckRun", "name": name, "status": status,
                           "conclusion": conclusion})
                    })
                    .collect();
                json!({"contexts": {"nodes": nodes}})
            });
        let threads: Vec<Value> = pr
            .threads
            .iter()
            .map(|t| {
                json!({"isResolved": t.resolved, "path": t.path, "line": t.line,
                       "comments": {"nodes": [{"author": {"__typename": "User", "login": t.author},
                                               "body": t.body}]}})
            })
            .collect();
        json!({"statusCheckRollup": rollup, "reviewThreads": {"nodes": threads}})
    }

    /// Set the CI checks on a Pull Request's current head.
    pub fn set_checks(&mut self, number: u64, checks: &[(&str, &str)]) {
        let head = self.branches()[&self.prs[&number].head_ref].clone();
        self.checks.insert(
            head,
            checks
                .iter()
                .map(|(name, state)| (name.to_string(), state.to_string()))
                .collect(),
        );
    }

    /// Record a review, replacing the reviewer's earlier one.
    pub fn review(&mut self, number: u64, login: &str, state: &str) {
        let pr = self.prs.get_mut(&number).expect("no such pull request");
        pr.reviews.retain(|(l, _)| l != login);
        pr.reviews.push((login.to_string(), state.to_string()));
    }

    pub fn add_thread(&mut self, number: u64, path: &str, line: u64, author: &str, body: &str) {
        let pr = self.prs.get_mut(&number).expect("no such pull request");
        pr.threads.push(ReviewThread {
            path: path.into(),
            line,
            author: author.into(),
            body: body.into(),
            resolved: false,
        });
    }

    // -- stacks ------------------------------------------------------------

    fn validate_stack(
        &self,
        admitted: &[u64],
        chained: &[u64],
        allowed_stack: Option<u64>,
    ) -> Result<(), ApiError> {
        let mut unique = chained.to_vec();
        unique.sort();
        unique.dedup();
        if unique.len() != chained.len() {
            return api_error(422, "Duplicate pull request.");
        }
        for n in admitted {
            match self.prs.get(n) {
                None => return api_error(422, "Pull request does not exist."),
                Some(pr) if !pr.is_open() => {
                    return api_error(422, "Pull request is not admissible.");
                }
                _ => {}
            }
        }
        for pair in chained.windows(2) {
            if self.prs[&pair[1]].base_ref != self.prs[&pair[0]].head_ref {
                return api_error(422, "Pull request bases do not form a chain.");
            }
        }
        for (number, members) in &self.stacks {
            if Some(*number) != allowed_stack && members.iter().any(|m| admitted.contains(m)) {
                return api_error(422, "Pull request already belongs to a stack.");
            }
        }
        Ok(())
    }

    fn create_stack(&mut self, members: Vec<u64>) -> Result<Value, ApiError> {
        if members.len() < 2 {
            return api_error(422, "A stack requires two pull requests.");
        }
        self.validate_stack(&members, &members, None)?;
        let number = self.next_number;
        self.next_number += 1;
        self.stacks.insert(number, members);
        Ok(self.stack_payload(number))
    }

    fn add_to_stack(&mut self, number: u64, added: Vec<u64>) -> Result<Value, ApiError> {
        let Some(existing) = self.stacks.get(&number).cloned() else {
            return api_error(404, "Not Found");
        };
        if added.is_empty() {
            return api_error(422, "No pull requests to append.");
        }
        // GitHub chains the added Pull Requests from the stack's last
        // member, even a closed one (seen live, not by the live script).
        let mut chained: Vec<u64> = existing.last().copied().into_iter().collect();
        chained.extend(&added);
        self.validate_stack(&added, &chained, Some(number))?;
        let mut members = existing;
        members.extend(added);
        self.stacks.insert(number, members);
        Ok(self.stack_payload(number))
    }

    fn unstack(&mut self, number: u64) -> Result<Option<Value>, ApiError> {
        let Some(members) = self.stacks.get(&number).cloned() else {
            return api_error(404, "Not Found");
        };
        let retained: Vec<u64> = members
            .into_iter()
            .filter(|n| self.prs[n].merged_at.is_some())
            .collect();
        if retained.is_empty() {
            self.stacks.remove(&number);
            Ok(None)
        } else {
            self.stacks.insert(number, retained);
            Ok(Some(self.stack_payload(number)))
        }
    }

    // -- merging -----------------------------------------------------------

    fn squash_merge(&mut self, number: u64, commit: Option<(String, String)>) -> Option<String> {
        self.squash_merge_from(number, None, commit)
    }

    /// Squash-merge `number` into its base; `None`, with nothing changed,
    /// when that conflicts. `merge_base` is where its own changes start,
    /// when that is not the merge base with the base branch (a stacked PR
    /// whose parent was just squash-merged below it). `commit` is the
    /// requested title and message; without it the message is the fake's
    /// own default.
    fn squash_merge_from(
        &mut self,
        number: u64,
        merge_base: Option<&str>,
        commit: Option<(String, String)>,
    ) -> Option<String> {
        let heads = self.branches();
        let (base_ref, head_ref, title) = {
            let pr = &self.prs[&number];
            (pr.base_ref.clone(), pr.head_ref.clone(), pr.title.clone())
        };
        let (base, head) = (heads[&base_ref].clone(), heads[&head_ref].clone());
        let message = match commit {
            Some((title, body)) if body.is_empty() => title,
            Some((title, body)) => format!("{title}\n\n{body}"),
            None => format!("{title} (#{number})"),
        };
        let tree = self.merged_tree(merge_base, &base, &head)?;
        let commit = self.commit_tree(&tree, &[&base], &message, None);
        self.git(&["update-ref", &format!("refs/heads/{base_ref}"), &commit]);
        let pr = self.prs.get_mut(&number).unwrap();
        pr.merged_at = Some(now());
        pr.merge_commit_sha = Some(commit.clone());
        pr.last_head_sha = head;
        pr.state = "closed".into();
        Some(commit)
    }

    /// Replay the non-merge commits of `old_base..head` onto `onto`, leaving
    /// out those that `onto` makes empty. Returns the new tip, which is
    /// `onto` when no commit is left, or `None` when a commit conflicts.
    fn replay(&self, head: &str, old_base: &str, onto: &str) -> Option<String> {
        let range = format!("{old_base}..{head}");
        let commits = self.git(&["rev-list", "--reverse", "--no-merges", &range]);
        let mut tip = onto.to_string();
        for commit in commits.lines() {
            let tree = self.merged_tree(Some(&format!("{commit}^")), &tip, commit)?;
            if tree == self.tree_of(&tip) {
                continue;
            }
            let message = self.git(&["log", "-1", "--format=%B", commit]);
            tip = self.commit_tree(&tree, &[&tip], &message, Some(commit));
        }
        Some(tip)
    }

    /// GitHub rebasing stack members, bottom first, after a merge below them
    /// or a press of the rebase button: the first one's own commits (those
    /// not in `old_base`) are replayed onto the head of `base_ref`, and each
    /// later one's onto the new head of the member below. A member left with
    /// no commits gets its new base's head and is closed, as live GitHub did
    /// with a reordered stack.
    ///
    /// When any replay conflicts, no branch is rewritten and this returns
    /// false, as live GitHub did after a conflict-resolved update. (Seen
    /// with the conflict in the first member; that a conflict further up
    /// also leaves the lower members alone is an assumption.)
    fn rebase_members(&mut self, members: &[u64], base_ref: &str, old_base: &str) -> bool {
        let heads = self.branches();
        let mut plan = Vec::new();
        let (mut onto, mut old) = (heads[base_ref].clone(), old_base.to_string());
        for &number in members {
            let head = heads[&self.prs[&number].head_ref].clone();
            let Some(tip) = self.replay(&head, &old, &onto) else {
                return false;
            };
            plan.push((number, tip.clone(), tip == onto));
            (onto, old) = (tip, head);
        }
        let mut new_base = base_ref.to_string();
        for (number, tip, emptied) in plan {
            let head_ref = self.prs[&number].head_ref.clone();
            self.git(&["update-ref", &format!("refs/heads/{head_ref}"), &tip]);
            let pr = self.prs.get_mut(&number).unwrap();
            pr.last_head_sha = tip;
            if emptied {
                pr.state = "closed".into();
                pr.closed_reason = Some("no commits left after the stack rebase".into());
            } else {
                pr.base_ref = new_base;
            }
            new_base = head_ref;
        }
        true
    }

    fn complete_stack_merge(&mut self, pr_number: u64) {
        let members = match self.stack_of(pr_number) {
            Some(stack) => self.active(&self.stacks[&stack].clone()),
            None => vec![pr_number],
        };
        let cut = members.iter().position(|n| *n == pr_number).unwrap() + 1;
        let (prefix, survivors) = (members[..cut].to_vec(), members[cut..].to_vec());
        let base_ref = self.prs[&prefix[0]].base_ref.clone();
        let old_top = self.branches()[&self.prs[&prefix[cut - 1]].head_ref].clone();
        let mut below: Option<String> = None;
        let commit = self.merge_ops[&pr_number].commit.clone();
        for number in &prefix {
            let head = self.branches()[&self.prs[number].head_ref].clone();
            let old_base = std::mem::replace(
                &mut self.prs.get_mut(number).unwrap().base_ref,
                base_ref.clone(),
            );
            let commit = commit.clone().filter(|_| *number == pr_number);
            if self
                .squash_merge_from(*number, below.as_deref(), commit)
                .is_none()
            {
                self.prs.get_mut(number).unwrap().base_ref = old_base;
                let op = self.merge_ops.get_mut(&pr_number).unwrap();
                op.status = "failed".into();
                op.message = format!("Pull request #{number} is not mergeable.");
                return;
            }
            below = Some(head);
        }
        if self.stale_head_polls > 0
            && let Some(next) = survivors.first()
        {
            self.prs.get_mut(next).unwrap().base_ref = base_ref.clone();
            self.held_rebase = Some((survivors, base_ref.clone(), old_top));
        } else if !self.rebase_members(&survivors, &base_ref, &old_top)
            && let Some(next) = survivors.first()
        {
            self.prs.get_mut(next).unwrap().base_ref = base_ref.clone();
        }
        let sha = self.branches()[&base_ref].clone();
        let op = self.merge_ops.get_mut(&pr_number).unwrap();
        op.status = "merged".into();
        op.message = "Pull request successfully merged.".into();
        op.sha = Some(sha);
    }

    /// Merge a Pull Request the way someone would in GitHub's web UI: a
    /// stacked PR merges its stack prefix and GitHub rebases the rest; any
    /// other PR is squash-merged into its base.
    pub fn merge_on_github(&mut self, number: u64) {
        if self.stack_of(number).is_some() {
            let head_ref = self.prs[&number].head_ref.clone();
            let head = self.branches()[&head_ref].clone();
            self.merge_ops.insert(
                number,
                MergeOperation {
                    uuid: format!("merge-{}", self.merge_ops.len() + 1),
                    pr_number: number,
                    method: "squash".into(),
                    expected_head: head,
                    commit: None,
                    status: "pending".into(),
                    message: "Merge request enqueued.".into(),
                    sha: None,
                },
            );
            self.complete_stack_merge(number);
            let status = &self.merge_ops[&number].status;
            assert_eq!(status, "merged", "fake github: #{number} did not merge");
        } else {
            self.squash_merge(number, None)
                .unwrap_or_else(|| panic!("fake github: #{number} is not mergeable"));
        }
    }

    /// The stack's rebase button in GitHub's web UI: replay every open
    /// member, bottom first, onto the current head of the stack's base.
    pub fn rebase_stack_on_github(&mut self, number: u64) {
        let stack = self.stack_of(number).expect("not in a stack");
        let members = self.active(&self.stacks[&stack].clone());
        let base_ref = self.prs[&members[0]].base_ref.clone();
        let heads = self.branches();
        let bottom_head = heads[&self.prs[&members[0]].head_ref].clone();
        let fork = self.git(&["merge-base", &bottom_head, &heads[&base_ref]]);
        self.rebase_members(&members, &base_ref, &fork);
    }

    /// Close a Pull Request without merging it, as in GitHub's web UI.
    pub fn close_on_github(&mut self, number: u64) {
        let pr = self.prs.get_mut(&number).expect("no such pull request");
        pr.state = "closed".into();
        pr.closed_reason = Some("closed on GitHub".into());
    }

    fn merge_payload(op: &MergeOperation) -> Value {
        json!({
            "status": op.status,
            "details": {
                "expected_head_sha": op.expected_head,
                "merge_action": "direct_merge",
                "merge_method": op.method,
                "message": op.message,
                "sha": op.sha,
                "uuid": op.uuid,
            },
        })
    }

    /// "Update branch": merge the base branch into an ordinary PR's branch.
    pub fn update_branch(&mut self, number: u64) {
        let heads = self.branches();
        let (base_ref, head_ref) = {
            let pr = &self.prs[&number];
            (pr.base_ref.clone(), pr.head_ref.clone())
        };
        let (base, head) = (heads[&base_ref].clone(), heads[&head_ref].clone());
        let message = format!("Merge branch '{base_ref}' into {head_ref}");
        let tree = self
            .merged_tree(None, &head, &base)
            .unwrap_or_else(|| panic!("fake github: #{number} conflicts with {base_ref}"));
        let commit = self.commit_tree(&tree, &[&head, &base], &message, None);
        self.git(&["update-ref", &format!("refs/heads/{head_ref}"), &commit]);
        self.prs.get_mut(&number).unwrap().last_head_sha = commit;
    }

    /// Open a Pull Request the way someone using GitHub directly would, for
    /// scenarios that do not start with jj-spr. Returns its number.
    pub fn open_pr_outside_spr(&mut self, head: &str, base: &str, title: &str) -> u64 {
        let path = format!("/repos/{}/{}/pulls", self.owner, self.name);
        let body = serde_json::json!({ "head": head, "base": base, "title": title });
        match self.route("POST", &path, "", &body) {
            Ok((_, Some(pr))) => pr["number"].as_u64().unwrap(),
            _ => panic!("could not open a Pull Request from {head} to {base}"),
        }
    }

    /// Register a stack the way someone using GitHub directly would.
    pub fn stack_outside_spr(&mut self, members: &[u64]) {
        if self.create_stack(members.to_vec()).is_err() {
            panic!("could not stack {members:?}");
        }
    }

    // -- routing -----------------------------------------------------------

    fn route(&mut self, method: &str, path: &str, query: &str, body: &Value) -> Reply {
        if path == "/graphql" && method == "POST" {
            return Ok((200, Some(self.graphql(body))));
        }
        let prefix = format!("/repos/{}/{}", self.owner, self.name);
        let Some(rest) = path.strip_prefix(&prefix) else {
            return api_error(404, "Not Found");
        };
        let rest = rest.to_string();
        let segments: Vec<&str> = rest.split('/').filter(|s| !s.is_empty()).collect();

        if segments.first() == Some(&"stacks") {
            if !self.stacks_enabled {
                return api_error(404, "Not Found");
            }
            return self.route_stacks(method, &segments, query, body);
        }

        match (method, segments.as_slice()) {
            ("POST", ["pulls"]) => {
                let head = body["head"].as_str().unwrap_or_default();
                if let Some(i) = self.failing_creations.iter().position(|b| b == head) {
                    self.failing_creations.remove(i);
                    return api_error(500, "fake github: creating this pull request failed");
                }
                let heads = self.branches();
                for key in ["head", "base"] {
                    let branch = body[key].as_str().unwrap_or_default();
                    if !heads.contains_key(branch) {
                        return api_error(422, format!("Branch {branch:?} does not exist."));
                    }
                }
                // Live GitHub refused this for the same head and base; other
                // bases were not tried.
                let base = body["base"].as_str().unwrap_or_default();
                if self
                    .prs
                    .values()
                    .any(|pr| pr.is_open() && pr.head_ref == head && pr.base_ref == base)
                {
                    return api_error(
                        422,
                        format!("A pull request already exists for {}:{head}.", self.owner),
                    );
                }
                let number = self.next_number;
                self.next_number += 1;
                let head_ref = body["head"].as_str().unwrap().to_string();
                self.prs.insert(
                    number,
                    PullRequest {
                        number,
                        title: body["title"].as_str().unwrap_or_default().to_string(),
                        body: body["body"].as_str().unwrap_or_default().to_string(),
                        last_head_sha: heads[&head_ref].clone(),
                        head_ref,
                        base_ref: body["base"].as_str().unwrap().to_string(),
                        draft: body["draft"].as_bool().unwrap_or(false),
                        state: "open".into(),
                        merged_at: None,
                        merge_commit_sha: None,
                        closed_reason: None,
                        reviews: Vec::new(),
                        threads: Vec::new(),
                    },
                );
                Ok((201, Some(self.pr_payload(number))))
            }
            ("GET", ["pulls", n]) => {
                let n = parse_number(n)?;
                self.pr_mut(n)?;
                Ok((200, Some(self.pr_payload(n))))
            }
            ("PATCH", ["pulls", n]) => {
                let n = parse_number(n)?;
                self.update_pr(n, body)?;
                Ok((200, Some(self.pr_payload(n))))
            }
            ("POST", ["pulls", n, "requested_reviewers"]) => {
                let n = parse_number(n)?;
                self.pr_mut(n)?;
                Ok((201, Some(self.pr_payload(n))))
            }
            ("PUT", ["pulls", n, "merge"]) => {
                let n = parse_number(n)?;
                let (head_ref, draft) = {
                    let pr = self.pr_mut(n)?;
                    (pr.head_ref.clone(), pr.draft)
                };
                if self.stack_of(n).is_some() {
                    return api_error(
                        403,
                        "Merging stacked PRs via this endpoint is not supported. Use the \
                         asynchronous merge endpoint instead.",
                    );
                }
                if draft {
                    return api_error(405, "Pull Request is still a draft");
                }
                if let Some(sha) = body["sha"].as_str()
                    && self.branches().get(&head_ref).map(String::as_str) != Some(sha)
                {
                    return api_error(409, "Head branch was modified.");
                }
                let Some(sha) = self.squash_merge(n, requested_commit(body)) else {
                    return api_error(405, "Pull Request is not mergeable");
                };
                Ok((
                    200,
                    Some(json!({"sha": sha, "merged": true,
                                "message": "Pull Request successfully merged"})),
                ))
            }
            ("PUT", ["pulls", n, "merge-async"]) => {
                let n = parse_number(n)?;
                let (head_ref, draft) = {
                    let pr = self.pr_mut(n)?;
                    (pr.head_ref.clone(), pr.draft)
                };
                if body["merge_action"].as_str() != Some("direct_merge") {
                    return api_error(400, "Merge action does not match policy.");
                }
                let expected = body["sha"].as_str().unwrap_or_default().to_string();
                if self.branches().get(&head_ref) != Some(&expected) {
                    return api_error(400, "Target head changed.");
                }
                let active = match self.stack_of(n) {
                    Some(stack) => self.active(&self.stacks[&stack].clone()),
                    None => vec![n],
                };
                if draft {
                    return Ok((
                        400,
                        Some(json!({"status": "failed",
                                    "details": {"message": "Pull request is in draft."}})),
                    ));
                }
                if !active.contains(&n) {
                    return api_error(400, "Target is not mergeable.");
                }
                let op = MergeOperation {
                    uuid: format!("merge-{}", self.merge_ops.len() + 1),
                    pr_number: n,
                    method: body["merge_method"].as_str().unwrap_or("merge").to_string(),
                    expected_head: expected,
                    commit: requested_commit(body),
                    status: "pending".into(),
                    message: "Merge request enqueued.".into(),
                    sha: None,
                };
                let payload = Self::merge_payload(&op);
                self.merge_ops.insert(n, op);
                Ok((202, Some(payload)))
            }
            ("PUT", ["pulls", n, "update-branch"]) => {
                let n = parse_number(n)?;
                self.pr_mut(n)?;
                if self.stack_of(n).is_some() {
                    return api_error(
                        403,
                        "Updating a stacked PR's branch via this endpoint is not supported.",
                    );
                }
                self.update_branch(n);
                Ok((
                    202,
                    Some(json!({"message": "Updating pull request branch.",
                                "url": self.web(&format!("/pull/{n}"))})),
                ))
            }
            ("GET", ["pulls", n, "merge-async", uuid]) => {
                let n = parse_number(n)?;
                let pending = match self.merge_ops.get(&n) {
                    Some(op) if op.uuid == *uuid => op.status == "pending",
                    _ => return api_error(404, "Not Found"),
                };
                if pending {
                    let pr_number = self.merge_ops[&n].pr_number;
                    self.complete_stack_merge(pr_number);
                }
                Ok((200, Some(Self::merge_payload(&self.merge_ops[&n]))))
            }
            _ => api_error(404, "Not Found"),
        }
    }

    fn update_pr(&mut self, number: u64, body: &Value) -> Result<(), ApiError> {
        let (base_ref, state) = {
            let pr = self.pr_mut(number)?;
            (pr.base_ref.clone(), pr.state.clone())
        };
        if let Some(new_base) = body["base"].as_str()
            && new_base != base_ref
        {
            if self.stack_of(number).is_some() {
                return Err(ApiError {
                    status: 422,
                    message: "Validation Failed".into(),
                    errors: vec![json!({
                        "resource": "PullRequest", "code": "invalid", "field": "base",
                        "message": "Cannot change the base branch because the pull request \
                                    is part of a stack.",
                    })],
                });
            }
            if state != "open" {
                return api_error(
                    422,
                    "Cannot change the base branch of a closed pull request.",
                );
            }
            if !self.branches().contains_key(new_base) {
                return api_error(422, format!("Branch {new_base:?} does not exist."));
            }
            self.pr_mut(number)?.base_ref = new_base.to_string();
        }
        let pr = self.pr_mut(number)?;
        if let Some(title) = body["title"].as_str() {
            pr.title = title.to_string();
        }
        if body.get("body").is_some() {
            pr.body = body["body"].as_str().unwrap_or_default().to_string();
        }
        if body["state"].as_str() == Some("closed") && pr.is_open() {
            pr.state = "closed".into();
            pr.closed_reason = Some("closed by request".into());
        }
        Ok(())
    }

    fn route_stacks(
        &mut self,
        method: &str,
        segments: &[&str],
        query: &str,
        body: &Value,
    ) -> Reply {
        let int_list = |body: &Value| -> Vec<u64> {
            body["pull_requests"]
                .as_array()
                .map(|a| a.iter().filter_map(Value::as_u64).collect())
                .unwrap_or_default()
        };
        match (method, segments) {
            ("GET", ["stacks"]) => {
                let params = parse_query(query);
                let page: u64 = params.get("page").and_then(|p| p.parse().ok()).unwrap_or(1);
                let wanted: Option<u64> = params.get("pull_request").and_then(|p| p.parse().ok());
                let numbers: Vec<u64> = if page == 1 {
                    self.stacks
                        .iter()
                        .filter(|(_, m)| wanted.is_none_or(|w| m.contains(&w)))
                        .map(|(n, _)| *n)
                        .collect()
                } else {
                    vec![]
                };
                let list: Vec<Value> = numbers.iter().map(|n| self.stack_payload(*n)).collect();
                Ok((200, Some(Value::Array(list))))
            }
            ("POST", ["stacks"]) => Ok((201, Some(self.create_stack(int_list(body))?))),
            ("GET", ["stacks", n]) => {
                let n = parse_number(n)?;
                if !self.stacks.contains_key(&n) {
                    return api_error(404, "Not Found");
                }
                Ok((200, Some(self.stack_payload(n))))
            }
            ("POST", ["stacks", n, "add"]) => {
                let n = parse_number(n)?;
                Ok((200, Some(self.add_to_stack(n, int_list(body))?)))
            }
            ("POST", ["stacks", n, "unstack"]) => {
                let n = parse_number(n)?;
                Ok(match self.unstack(n)? {
                    None => (204, None),
                    Some(remaining) => (200, Some(remaining)),
                })
            }
            _ => api_error(404, "Not Found"),
        }
    }

    fn graphql(&mut self, body: &Value) -> Value {
        let name = body["operationName"].as_str().unwrap_or_default();
        let variables = &body["variables"];
        if self.failing_queries.iter().any(|q| q == name) {
            return json!({"errors": [{"message": format!("fake github: {name} failed")}]});
        }
        match name {
            "PullRequestQuery" | "PullRequestMergeabilityQuery" => {
                if name == "PullRequestMergeabilityQuery" && self.held_rebase.is_some() {
                    if self.stale_head_polls > 0 {
                        self.stale_head_polls -= 1;
                    } else if let Some((members, base_ref, old_top)) = self.held_rebase.take() {
                        self.rebase_members(&members, &base_ref, &old_top);
                    }
                }
                let mut data = variables["number"]
                    .as_u64()
                    .filter(|n| self.prs.contains_key(n))
                    .map(|n| self.graphql_pr(n));
                if name == "PullRequestMergeabilityQuery"
                    && self.unknown_mergeability_polls > 0
                    && let Some(pr) = &mut data
                {
                    self.unknown_mergeability_polls -= 1;
                    pr["mergeable"] = json!("UNKNOWN");
                    pr["mergeStateStatus"] = json!("UNKNOWN");
                }
                json!({"data": {"repository": {"pullRequest": data}}})
            }
            "PullRequestStatusQuery" => {
                let data = variables["number"]
                    .as_u64()
                    .filter(|n| self.prs.contains_key(n))
                    .map(|n| self.graphql_pr_status(n));
                json!({"data": {"repository": {"pullRequest": data}}})
            }
            "OpenPullRequestBranchesQuery" => {
                let nodes: Vec<Value> = self
                    .prs
                    .values()
                    .filter(|pr| pr.is_open())
                    .map(|pr| {
                        json!({"number": pr.number, "headRefName": pr.head_ref,
                               "baseRefName": pr.base_ref})
                    })
                    .collect();
                json!({"data": {"repository": {"pullRequests": {
                    "nodes": nodes, "pageInfo": {"hasNextPage": false, "endCursor": null}}}}})
            }
            "PullRequestsForBranchQuery" => {
                let head = variables["head"].as_str().unwrap_or_default();
                let nodes: Vec<Value> = self
                    .prs
                    .values()
                    .filter(|pr| pr.head_ref == head)
                    .map(|pr| {
                        let state = match (pr.is_open(), pr.merged_at.is_some()) {
                            (true, _) => "OPEN",
                            (false, true) => "MERGED",
                            (false, false) => "CLOSED",
                        };
                        json!({"number": pr.number, "state": state, "isCrossRepository": false})
                    })
                    .collect();
                json!({"data": {"repository": {"pullRequests": {"nodes": nodes}}}})
            }
            "SearchQuery" => {
                let nodes: Vec<Value> = self
                    .prs
                    .values()
                    .filter(|pr| pr.is_open())
                    .map(|pr| {
                        json!({"__typename": "PullRequest", "number": pr.number,
                               "title": pr.title, "url": self.web(&format!("/pull/{}", pr.number)),
                               "reviewDecision": null})
                    })
                    .collect();
                json!({"data": {"search": {"nodes": nodes}}})
            }
            other => {
                json!({"errors": [{"message": format!("fake github: unknown operation {other:?}")}]})
            }
        }
    }
}

/// The `commit_title` and `commit_message` of a merge request. GitHub
/// uses its default for whichever is missing; the fake uses its own default
/// unless a title is given.
fn requested_commit(body: &Value) -> Option<(String, String)> {
    let title = body["commit_title"].as_str()?;
    let message = body["commit_message"].as_str().unwrap_or_default();
    Some((title.to_string(), message.to_string()))
}

fn parse_number(text: &str) -> Result<u64, ApiError> {
    text.parse().or_else(|_| api_error(404, "Not Found"))
}

fn parse_query(query: &str) -> HashMap<String, String> {
    query
        .split('&')
        .filter_map(|pair| pair.split_once('='))
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
}

/// Make pushes to the repository at `git_dir` refuse a ref name longer than
/// 255 bytes, as GitHub's pre-receive hook does (error GH005).
fn refuse_long_refs(git_dir: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let hook = git_dir.join("hooks").join("pre-receive");
    std::fs::create_dir_all(hook.parent().unwrap()).expect("hooks directory");
    std::fs::write(
        &hook,
        "#!/bin/sh\n\
         while read old new ref; do\n\
         \x20 bytes=$(printf %s \"$ref\" | wc -c)\n\
         \x20 if [ $bytes -gt 255 ]; then\n\
         \x20   echo \"error: GH005: Sorry, refs longer than 255 bytes are not allowed.\" >&2\n\
         \x20   exit 1\n\
         \x20 fi\n\
         done\n",
    )
    .expect("pre-receive hook");
    std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).expect("hook mode");
}

/// A running fake GitHub. The server stops when this is dropped.
pub struct FakeGitHub {
    state: Arc<Mutex<State>>,
    shutdown: Option<tokio::sync::oneshot::Sender<()>>,
    thread: Option<JoinHandle<()>>,
    pub url: String,
}

impl FakeGitHub {
    pub fn start(owner: &str, name: &str, git_dir: &Path, stacks_enabled: bool) -> Self {
        refuse_long_refs(git_dir);
        let state = Arc::new(Mutex::new(State {
            owner: owner.into(),
            name: name.into(),
            git_dir: git_dir.to_path_buf(),
            stacks_enabled,
            prs: BTreeMap::new(),
            stacks: BTreeMap::new(),
            checks: HashMap::new(),
            merge_ops: HashMap::new(),
            failing_queries: Vec::new(),
            unknown_mergeability_polls: 0,
            stale_head_polls: 0,
            held_rebase: None,
            blocked: Vec::new(),
            failing_creations: Vec::new(),
            requests: Vec::new(),
            next_number: 1,
        }));

        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind fake github");
        listener
            .set_nonblocking(true)
            .expect("nonblocking listener");
        let url = format!("http://{}", listener.local_addr().expect("local address"));
        let (shutdown, stopped) = tokio::sync::oneshot::channel::<()>();

        let thread = {
            let state = state.clone();
            std::thread::spawn(move || {
                let runtime = tokio::runtime::Builder::new_multi_thread()
                    .worker_threads(2)
                    .enable_all()
                    .build()
                    .expect("tokio runtime");
                runtime.block_on(async move {
                    let listener =
                        tokio::net::TcpListener::from_std(listener).expect("tokio listener");
                    let app = axum::Router::new().fallback(handle).with_state(state);
                    axum::serve(listener, app)
                        .with_graceful_shutdown(async {
                            let _ = stopped.await;
                        })
                        .await
                        .expect("fake github server");
                });
            })
        };

        Self {
            state,
            shutdown: Some(shutdown),
            thread: Some(thread),
            url,
        }
    }

    /// Lock the fake's state for inspection (or for arranging a scenario).
    pub fn state(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }
}

impl Drop for FakeGitHub {
    fn drop(&mut self) {
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(());
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// Every request goes through `State::route`.
async fn handle(
    axum::extract::State(state): axum::extract::State<Arc<Mutex<State>>>,
    method: Method,
    uri: Uri,
    body: Bytes,
) -> Response {
    let method = method.as_str().to_string();
    let path = uri.path().to_string();
    let query = uri.query().unwrap_or_default().to_string();
    let body: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);

    // The fake shells out to git, so do the work off the async runtime.
    let (status, payload) = tokio::task::spawn_blocking(move || {
        let mut state = state.lock().unwrap_or_else(|e| e.into_inner());
        state.requests.push(Recorded {
            method: method.clone(),
            path: path.clone(),
            body: body.clone(),
        });
        state.refresh();
        let reply = state.route(&method, &path, &query, &body);
        state.refresh();
        match reply {
            Ok(reply) => reply,
            Err(error) => {
                let mut payload =
                    json!({"message": error.message, "status": error.status.to_string()});
                if !error.errors.is_empty() {
                    payload["errors"] = Value::Array(error.errors);
                }
                (error.status, Some(payload))
            }
        }
    })
    .await
    .expect("fake github handler panicked");

    let status = StatusCode::from_u16(status).expect("valid status");
    let body = payload.map(|p| p.to_string()).unwrap_or_default();
    (status, [(header::CONTENT_TYPE, "application/json")], body).into_response()
}
