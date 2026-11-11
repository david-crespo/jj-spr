//! End-to-end tests of Pull Requests made of several local changes, against
//! a fake GitHub (see `common/fake_github.rs`). They need `jj` and `git` on
//! `PATH`. Set `SPR_TEST_VERBOSE=1` to print every command.

mod common;

use common::env::TestEnv;

fn heads(env: &TestEnv, prs: &[u64]) -> Vec<String> {
    prs.iter().map(|&n| env.head_of(n)).collect()
}

/// A, B (with b.txt and b2.txt) and C, submitted, then B split in two so
/// both halves keep B's description and its `Pull Request:` line. Returns
/// the changes bottom first (A, lower half, upper half, C) and the PRs of
/// A, B and C.
fn stack_with_split_middle(env: &TestEnv) -> (Vec<String>, Vec<u64>) {
    let a = env.commit("Change A", "a.txt", "a\n");
    std::fs::write(env.work.join("b2.txt"), "b2\n").unwrap();
    let b = env.commit("Change B", "b.txt", "b\n");
    let c = env.commit("Change C", "c.txt", "c\n");
    env.spr(&["diff", "--all", "-m", "initial"]);
    let prs = env.prs_for(&[a.clone(), b.clone(), c.clone()]);
    // Accept the description unchanged, as `jj split` does in an editor.
    env.jj(&[
        "--config",
        "ui.editor=\"true\"",
        "split",
        "-r",
        &b,
        "b2.txt",
    ]);
    let lower = env.change_id(&format!("parents({c})-"));
    let upper = env.change_id(&format!("parents({c})"));
    (vec![a, lower, upper, c], prs)
}

fn description(env: &TestEnv, change: &str) -> String {
    env.jj_query(&["log", "--no-graph", "-r", change, "-T", "description"])
}

#[test]
fn squashed_halves_keep_their_pull_request() {
    // `jj squash` of two changes with the same `Pull Request:` line keeps
    // the line twice. That still names one Pull Request.
    let env = TestEnv::new();
    let (changes, prs) = stack_with_split_middle(&env);
    env.jj(&[
        "--config",
        "ui.editor=\"true\"",
        "squash",
        "--from",
        &changes[2],
        "--into",
        &changes[1],
    ]);
    assert_eq!(
        description(&env, &changes[1])
            .matches("Pull Request:")
            .count(),
        2
    );
    let before = heads(&env, &prs);

    let out = env.spr(&["diff", "--all", "-m", "after squash"]);
    assert!(!out.text.contains("Created new"), "{}", out.text);
    assert_eq!(heads(&env, &prs), before, "nothing should change");
    assert_eq!(env.pr_for(&changes[1]), prs[1]);
}

fn local_tree(env: &TestEnv, change: &str) -> String {
    env.tree_of(change)
}

fn pr_tree(env: &TestEnv, number: u64) -> String {
    env.remote_git(&["rev-parse", &format!("{}^{{tree}}", env.head_of(number))])
}

/// The subjects of the commits on a Pull Request's branch that are not on
/// its base branch, oldest first.
fn pr_commits(env: &TestEnv, number: u64) -> Vec<String> {
    let pr = env.pr(number);
    env.remote_git(&[
        "log",
        "--reverse",
        "--format=%s",
        &format!("{}..{}", pr.base_ref, pr.head_ref),
    ])
    .lines()
    .map(str::to_string)
    .collect()
}

/// A change on top of `below` that links `below`'s Pull Request.
fn commit_into_pull_request(env: &TestEnv, below: &str, title: &str, path: &str) -> String {
    let link = description(env, below)
        .lines()
        .find(|line| line.starts_with("Pull Request:"))
        .expect("a Pull Request line")
        .to_string();
    env.commit(&format!("{title}\n\n{link}"), path, &format!("{path}\n"))
}

/// A Pull Request is open, targets `base`, shows `files`, and has the tree
/// of `top`, its top change.
fn check_pr(env: &TestEnv, number: u64, base: &str, files: &[&str], top: &str) {
    let pr = env.pr(number);
    assert!(pr.is_open(), "PR #{number} is {}", pr.state);
    assert_eq!(pr.base_ref, base, "PR #{number}'s base");
    assert_eq!(env.pr_files(number), files, "files in PR #{number}'s diff");
    assert_eq!(
        pr_tree(env, number),
        local_tree(env, top),
        "PR #{number}'s tree"
    );
}

#[test]
fn split_of_a_submitted_change_updates_nothing() {
    // papertigers/jj-spr#12: after `jj split`, both halves updated the same
    // PR, which ended up based on its own branch. They are one PR.
    let env = TestEnv::new();
    let (changes, prs) = stack_with_split_middle(&env);
    let before = heads(&env, &prs);
    let stacks = env.active_stacks();

    let out = env.spr(&["diff", "--all", "-m", "after split"]);
    assert_eq!(
        out.text.matches("No update necessary").count(),
        3,
        "{}",
        out.text
    );
    assert_eq!(heads(&env, &prs), before, "nothing should be pushed");
    assert_eq!(env.active_stacks(), stacks);
    assert_eq!(env.pr_for(&changes[1]), prs[1]);
    assert_eq!(env.pr_for(&changes[2]), prs[1]);
}

#[test]
fn either_half_alone_is_the_whole_pull_request() {
    let env = TestEnv::new();
    let (changes, prs) = stack_with_split_middle(&env);
    let before = heads(&env, &prs);
    for half in &changes[1..3] {
        let out = env.spr(&["diff", "-r", half, "-m", "one half"]);
        assert!(out.text.contains("No update necessary"), "{}", out.text);
    }
    // A range that ends in the middle of the PR, and one that starts there.
    for range in [
        format!("{}::{}", changes[0], changes[1]),
        format!("{}::{}", changes[2], changes[3]),
    ] {
        let out = env.spr(&["diff", "-r", &range, "-m", "part"]);
        assert!(!out.text.contains("Created new"), "{range}: {}", out.text);
    }
    assert_eq!(heads(&env, &prs), before, "nothing should be pushed");
}

fn amend_lower_half(stacks_enabled: bool) {
    let env = TestEnv::with_stacks(stacks_enabled);
    if !stacks_enabled {
        env.jj(&["config", "set", "--repo", "spr.nativeStacks", "false"]);
    }
    let (changes, prs) = stack_with_split_middle(&env);
    let old = env.head_of(prs[1]);
    env.jj(&["edit", &changes[1]]);
    std::fs::write(env.work.join("b2.txt"), "b2, revised\n").unwrap();
    env.jj(&["new", &changes[3]]);

    env.spr(&["diff", "--all", "-m", "review feedback"]);
    let new = env.head_of(prs[1]);
    assert_eq!(
        env.remote_git(&[
            "rev-list",
            "--count",
            &format!("{old}..{new}"),
            "--no-merges"
        ]),
        "1",
        "one appended commit"
    );
    assert_eq!(
        env.remote_git(&["log", "-1", "--format=%s", &new]),
        "review feedback"
    );
    assert_eq!(env.pr_files(prs[1]), vec!["b.txt", "b2.txt"]);
    assert_eq!(pr_tree(&env, prs[1]), local_tree(&env, &changes[2]));
    assert_eq!(env.pr_files(prs[2]), vec!["c.txt"]);
}

#[test]
fn amending_the_lower_half_appends_one_commit() {
    amend_lower_half(true);
}

#[test]
fn amending_the_lower_half_appends_one_commit_legacy() {
    amend_lower_half(false);
}

#[test]
fn a_change_between_two_members_joins_them() {
    let env = TestEnv::new();
    let (changes, prs) = stack_with_split_middle(&env);
    env.jj(&["new", "--insert-after", &changes[1], "-m", "Between"]);
    std::fs::write(env.work.join("b3.txt"), "b3\n").unwrap();
    let between = env.change_id("@");
    env.jj(&["new", &changes[3]]);

    let out = env.spr(&["diff", "--all", "-m", "add b3"]);
    assert!(!out.text.contains("Created new"), "{}", out.text);
    assert_eq!(
        env.pr_for(&between),
        prs[1],
        "the new change joins #{}",
        prs[1]
    );
    check_pr(
        &env,
        prs[1],
        &env.pr(prs[0]).head_ref,
        &["b.txt", "b2.txt", "b3.txt"],
        &changes[2],
    );
    check_pr(
        &env,
        prs[2],
        &env.pr(prs[1]).head_ref,
        &["c.txt"],
        &changes[3],
    );
}

#[test]
fn a_new_change_above_the_top_member_gets_its_own_pull_request() {
    let env = TestEnv::new();
    let (changes, prs) = stack_with_split_middle(&env);
    env.jj(&["new", "--insert-after", &changes[2], "-m", "Above"]);
    std::fs::write(env.work.join("d.txt"), "d\n").unwrap();
    let above = env.change_id("@");
    env.jj(&["new", &changes[3]]);

    env.spr(&["diff", "--all", "-m", "add d"]);
    let new = env.pr_for(&above);
    assert!(!prs.contains(&new));
    check_pr(&env, new, &env.pr(prs[1]).head_ref, &["d.txt"], &above);
    check_pr(&env, prs[2], &env.pr(new).head_ref, &["c.txt"], &changes[3]);
}

#[test]
fn changes_with_one_pull_request_that_are_not_adjacent_are_refused() {
    let env = TestEnv::new();
    let changes = env.build_stack(&["a", "b"]);
    let prs = env.prs_for(&changes);
    // A copy of A on top of B links A's PR, but B is between them.
    env.jj(&["duplicate", &changes[0], "--onto", &changes[1]]);
    let copy = env.change_id(&format!(
        "children({}) & description(substring:\"Change A\")",
        changes[1]
    ));
    env.jj(&["new", &copy]);
    let before = heads(&env, &prs);

    let out = env.try_spr(&["diff", "--all", "-m", "copy"]);
    assert!(!out.success, "diff should refuse:\n{}", out.text);
    assert!(out.flat().contains(&format!("#{}", prs[0])), "{}", out.text);
    assert!(out.text.contains(&changes[0][..8]), "{}", out.text);
    assert!(out.text.contains(&copy[..8]), "{}", out.text);
    assert_eq!(heads(&env, &prs), before, "nothing may be pushed");
}

#[test]
fn range_above_another_pull_request_stacks_the_run_on_it() {
    // A range whose bottom is a member of a run that sits on another PR.
    let env = TestEnv::new();
    let (changes, prs) = stack_with_split_middle(&env);
    env.jj(&["edit", &changes[2]]);
    std::fs::write(env.work.join("b.txt"), "b, revised\n").unwrap();
    env.jj(&["new", &changes[3]]);

    env.spr(&[
        "diff",
        "-r",
        &format!("{}::{}", changes[1], changes[3]),
        "-m",
        "range",
    ]);
    check_pr(
        &env,
        prs[1],
        &env.pr(prs[0]).head_ref,
        &["b.txt", "b2.txt"],
        &changes[2],
    );
}

#[test]
fn stack_shows_one_row_per_pull_request() {
    let env = TestEnv::new();
    let (changes, prs) = stack_with_split_middle(&env);
    let out = env.spr(&["stack"]);
    let rows = out.text.lines().filter(|line| line.starts_with('○'));
    let b_rows = rows.filter(|line| line.contains(&format!("#{}", prs[1])));
    assert_eq!(b_rows.count(), 1, "{}", out.text);
    assert!(out.text.contains(&changes[1][..8]), "{}", out.text);
    assert!(
        out.text.contains("Everything matches GitHub"),
        "{}",
        out.text
    );
    assert!(out.text.contains(&changes[2][..8]), "{}", out.text);
    let json = env.spr(&["stack", "--json"]);
    assert!(json.text.contains(&changes[1][..8]), "{}", json.text);
}

#[test]
fn a_broken_link_elsewhere_does_not_stop_other_changes() {
    // `jj duplicate` of a submitted change: two changes link its Pull
    // Request without being one run.
    let env = TestEnv::new();
    let a = env.commit("Change A", "a.txt", "a\n");
    env.spr(&["diff", "-r", &a, "-m", "A"]);
    env.jj(&["duplicate", &a]);
    env.jj(&["new", "main@origin"]);
    let c = env.commit("Change C", "c.txt", "c\n");

    env.spr(&["diff", "-r", &c, "-m", "C"]);
    assert_ne!(env.pr_for(&c), env.pr_for(&a));
    let out = env.try_spr(&["diff", "-r", &a, "-m", "A"]);
    assert!(!out.success, "diff should refuse:\n{}", out.text);
    assert!(out.flat().contains("not one run"), "{}", out.text);
}

#[test]
fn stack_refuses_a_merge_inside_a_pull_request() {
    let env = TestEnv::new();
    let (changes, _) = stack_with_split_middle(&env);
    // A merge whose other parent is already on main, so the line of changes
    // up to the target has nothing on a separate branch.
    env.jj(&[
        "rebase",
        "-s",
        &changes[1],
        "-d",
        &changes[0],
        "-d",
        "main@origin",
    ]);
    env.jj(&["new", &changes[3]]);
    let out = env.try_spr(&["stack"]);
    assert!(!out.success, "stack should refuse:\n{}", out.text);
    assert!(out.flat().contains("is a merge"), "{}", out.text);
}

#[test]
fn sync_abandons_every_member_of_a_merged_pull_request() {
    let env = TestEnv::new();
    let (changes, prs) = stack_with_split_middle(&env);
    env.gh.state().merge_on_github(prs[1]);

    env.spr(&["sync", "-r", &changes[3]]);
    for change in &changes[..3] {
        assert!(!env.is_visible(change), "{change} should be abandoned");
    }
    assert_eq!(env.parent_of(&changes[3]), env.commit_id("main@origin"));
    env.spr(&["diff", "--all", "-m", "after sync"]);
    check_pr(&env, prs[2], "main", &["c.txt"], &changes[3]);
}

#[test]
fn sync_rebases_a_remaining_run_from_its_bottom_member() {
    let env = TestEnv::new();
    let (changes, prs) = stack_with_split_middle(&env);
    env.gh.state().merge_on_github(prs[0]);

    env.spr(&["sync", "-r", &changes[3]]);
    assert!(!env.is_visible(&changes[0]));
    assert_eq!(env.parent_of(&changes[1]), env.commit_id("main@origin"));
    env.spr(&["diff", "--all", "-m", "after sync"]);
    check_pr(&env, prs[1], "main", &["b.txt", "b2.txt"], &changes[2]);
    check_pr(
        &env,
        prs[2],
        &env.pr(prs[1]).head_ref,
        &["c.txt"],
        &changes[3],
    );
}

#[test]
fn diff_allows_github_rebase_of_a_run_after_stacked_merge() {
    // pr_has_remote_changes' evolog clause, for a PR of two changes:
    // GitHub's rebase after a stacked merge is nobody else's work.
    let env = TestEnv::new();
    let (changes, prs) = stack_with_split_middle(&env);
    env.spr(&["land", "-r", &changes[0]]);
    env.spr(&["sync", "-r", &changes[3]]);
    env.jj(&["edit", &changes[2]]);
    std::fs::write(env.work.join("b.txt"), "b, edited after sync\n").unwrap();
    env.jj(&["new", &changes[3]]);

    env.spr(&["diff", "--all", "-m", "after sync"]);
    check_pr(&env, prs[1], "main", &["b.txt", "b2.txt"], &changes[2]);
}

#[test]
fn close_removes_the_pull_request_line_from_every_member() {
    let env = TestEnv::new();
    let (changes, prs) = stack_with_split_middle(&env);
    env.spr(&["close", "-r", &changes[1]]);
    assert!(!env.pr(prs[1]).is_open());
    for half in &changes[1..3] {
        assert!(
            !description(&env, half).contains("Pull Request:"),
            "{half}: {}",
            description(&env, half)
        );
    }
}

#[test]
fn patch_puts_github_edits_into_the_top_member() {
    let env = TestEnv::new();
    let (changes, prs) = stack_with_split_middle(&env);
    env.push_to_pr_branch(prs[1], "b-fix.txt", "fix\n");
    let lower = env.commit_id(&changes[1]);

    env.spr(&["patch", &prs[1].to_string(), "--no-stack"]);
    assert_eq!(
        env.commit_id(&changes[1]),
        lower,
        "the lower member is unchanged"
    );
    assert!(env.files_in(&changes[2]).contains(&"b-fix.txt".to_string()));
    env.spr(&["diff", "--all", "-m", "after patch"]);
    assert_eq!(env.pr_files(prs[1]), vec!["b-fix.txt", "b.txt", "b2.txt"]);
}

#[test]
fn land_from_any_member() {
    let env = TestEnv::new();
    let (changes, prs) = stack_with_split_middle(&env);
    env.spr(&["land", "-r", &changes[1], "--yes"]);
    assert!(env.pr(prs[0]).merged_at.is_some());
    assert!(env.pr(prs[1]).merged_at.is_some());
    assert!(env.pr(prs[2]).is_open());
}

#[test]
fn the_pull_request_message_comes_from_the_top_member() {
    let env = TestEnv::new();
    let (changes, _) = stack_with_split_middle(&env);
    let retitle = |change: &str, title: &str| {
        let text = description(&env, change).replace("Change B", title);
        env.jj(&["describe", "-r", change, "-m", &text]);
    };
    retitle(&changes[1], "Lower half of B");
    let out = env.spr(&["diff", "--all", "-m", "retitled lower"]);
    assert!(!out.text.contains("differ"), "{}", out.text);

    retitle(&changes[2], "Upper half of B");
    let out = env.spr(&["diff", "--all", "-m", "retitled upper"]);
    assert!(out.flat().contains("differ"), "{}", out.text);
    assert!(
        out.flat().contains("the top of its 2 changes"),
        "{}",
        out.text
    );
    // `spr amend` would overwrite the top change's description.
    assert!(!out.flat().contains("spr amend"), "{}", out.text);
}

#[test]
fn a_conflicted_lower_member_is_refused() {
    let env = TestEnv::new();
    let (changes, prs) = stack_with_split_middle(&env);
    env.push_to_main("b2.txt", "someone else's b2\n");
    env.jj(&["git", "fetch"]);
    env.jj(&["rebase", "-s", &changes[0], "-d", "main@origin"]);
    let before = heads(&env, &prs);

    let out = env.try_spr(&["diff", "--all", "-m", "after rebase"]);
    assert!(!out.success, "diff should refuse:\n{}", out.text);
    assert!(out.flat().contains("conflicts"), "{}", out.text);
    assert!(out.text.contains(&changes[1][..8]), "{}", out.text);
    assert_eq!(heads(&env, &prs), before, "nothing may be pushed");
}

#[test]
fn moving_a_pull_request_above_the_one_on_it_appends_its_steps() {
    // P (B1, B2) with Q (C) on it, then reordered to C, B1, B2: P's old head
    // is in Q's new head, so P's new commits go on Q's head alone.
    let env = TestEnv::new();
    let b1 = env.commit("Change b1", "b1.txt", "b1\n");
    env.spr(&["diff", "-r", &b1, "-m", "unused"]);
    let b2 = commit_into_pull_request(&env, &b1, "Change b2", "b2.txt");
    let c = env.commit("Change C", "c.txt", "c\n");
    env.spr(&["diff", "--all", "-m", "unused"]);
    let (p, q) = (env.pr_for(&b2), env.pr_for(&c));

    env.jj(&["rebase", "-r", &c, "-d", "main@origin"]);
    env.jj(&["rebase", "-s", &b1, "-d", &c]);
    env.jj(&["new", &b2]);
    env.spr(&["diff", "--all", "-m", "reorder"]);
    check_pr(&env, q, "main", &["c.txt"], &c);
    check_pr(&env, p, &env.pr(q).head_ref, &["b1.txt", "b2.txt"], &b2);
    assert_eq!(pr_commits(&env, p), vec!["Change b1", "Change b2"]);
}

fn change_added_on_top(stacks_enabled: bool) {
    let env = TestEnv::with_stacks(stacks_enabled);
    if !stacks_enabled {
        env.jj(&["config", "set", "--repo", "spr.nativeStacks", "false"]);
    }
    let changes = env.build_stack(&["a", "b"]);
    let prs = env.prs_for(&changes);
    let before = pr_commits(&env, prs[1]);
    env.jj(&["new", &changes[1]]);
    let top = commit_into_pull_request(&env, &changes[1], "More of B", "b2.txt");

    env.spr(&["diff", "--all", "-m", "unused"]);
    let mut expected = before;
    expected.push("More of B".to_string());
    assert_eq!(pr_commits(&env, prs[1]), expected);
    check_pr(
        &env,
        prs[1],
        &env.pr(prs[1]).base_ref,
        &["b.txt", "b2.txt"],
        &top,
    );
    // The Pull Request keeps the title it was opened with, which is no
    // longer its top change's. That is not reported as a difference.
    assert_eq!(env.pr(prs[1]).title, "Change B");
    let out = env.spr(&["diff", "--all", "-m", "again"]);
    assert_eq!(
        out.text.matches("No update necessary").count(),
        2,
        "{}",
        out.text
    );
    assert!(!out.text.contains("differ"), "{}", out.text);
}

#[test]
fn a_change_added_on_top_is_appended_as_its_own_commit() {
    change_added_on_top(true);
}

#[test]
fn a_change_added_on_top_is_appended_as_its_own_commit_legacy() {
    change_added_on_top(false);
}

/// A, submitted alone, then B1, B2 and B3 above it, submitted with
/// `--one-pr`.
fn one_pr_above_another(stacks_enabled: bool) {
    let env = TestEnv::with_stacks(stacks_enabled);
    if !stacks_enabled {
        env.jj(&["config", "set", "--repo", "spr.nativeStacks", "false"]);
    }
    let a = env.commit("Change A", "a.txt", "a\n");
    env.spr(&["diff", "-r", &a, "-m", "A"]);
    let pa = env.pr_for(&a);
    let b: Vec<String> = ["b1", "b2", "b3"]
        .iter()
        .map(|name| env.commit(&format!("Change {name}"), &format!("{name}.txt"), "b\n"))
        .collect();

    let range = format!("{}::{}", b[0], b[2]);
    env.spr(&["diff", "-r", &range, "--one-pr", "-m", "unused"]);
    let pb = env.pr_for(&b[2]);
    for change in &b {
        assert_eq!(env.pr_for(change), pb, "{change} links the Pull Request");
    }
    let pr = env.pr(pb);
    assert_eq!(pr.title, "Change b3");
    if stacks_enabled {
        assert_eq!(pr.base_ref, env.pr(pa).head_ref);
    }
    check_pr(
        &env,
        pb,
        &pr.base_ref,
        &["b1.txt", "b2.txt", "b3.txt"],
        &b[2],
    );
    assert_eq!(
        pr_commits(&env, pb),
        vec!["Change b1", "Change b2", "Change b3"]
    );

    let out = env.spr(&["diff", "--all", "-m", "again"]);
    assert_eq!(
        out.text.matches("No update necessary").count(),
        2,
        "{}",
        out.text
    );
    assert!(!out.text.contains("Created new"), "{}", out.text);
}

#[test]
fn one_pr_opens_one_pull_request_with_a_commit_per_change() {
    one_pr_above_another(true);
}

#[test]
fn one_pr_opens_one_pull_request_with_a_commit_per_change_legacy() {
    one_pr_above_another(false);
}

#[test]
fn one_pr_on_main() {
    let env = TestEnv::new();
    let a1 = env.commit("Change a1", "a1.txt", "a1\n");
    let a2 = env.commit("Change a2", "a2.txt", "a2\n");
    env.spr(&["diff", "--all", "--one-pr", "-m", "unused"]);
    let pr = env.pr_for(&a2);
    assert_eq!(env.pr_for(&a1), pr);
    check_pr(&env, pr, "main", &["a1.txt", "a2.txt"], &a2);
    assert_eq!(pr_commits(&env, pr), vec!["Change a1", "Change a2"]);
}

#[test]
fn one_pr_adds_new_changes_to_a_pull_request() {
    let env = TestEnv::new();
    let a = env.commit("Change A", "a.txt", "a\n");
    env.spr(&["diff", "-r", &a, "-m", "first"]);
    let pr = env.pr_for(&a);
    let more = env.commit("More of A", "a2.txt", "a2\n");
    let most = env.commit("Most of A", "a3.txt", "a3\n");

    env.spr(&[
        "diff",
        "-r",
        &format!("{a}::{most}"),
        "--one-pr",
        "-m",
        "unused",
    ]);
    assert_eq!(env.pr_for(&more), pr);
    assert_eq!(env.pr_for(&most), pr);
    assert_eq!(env.pr(pr).title, "Change A", "the title is not updated");
    check_pr(&env, pr, "main", &["a.txt", "a2.txt", "a3.txt"], &most);
    assert_eq!(
        pr_commits(&env, pr),
        vec!["first", "More of A", "Most of A"]
    );
}

#[test]
fn one_pr_refuses_changes_with_different_pull_requests() {
    let env = TestEnv::new();
    let changes = env.build_stack(&["a", "b"]);
    let prs = env.prs_for(&changes);
    let before = heads(&env, &prs);
    let out = env.try_spr(&["diff", "--all", "--one-pr", "-m", "join"]);
    assert!(!out.success, "diff should refuse:\n{}", out.text);
    assert!(out.flat().contains(&format!("#{}", prs[0])), "{}", out.text);
    assert!(out.flat().contains(&format!("#{}", prs[1])), "{}", out.text);
    assert_eq!(heads(&env, &prs), before, "nothing may be pushed");
}

#[test]
fn one_pr_refuses_changes_without_a_description() {
    // Each change becomes a commit on the Pull Request branch, so it needs
    // a message.
    let env = TestEnv::new();
    let b1 = env.commit("", "b1.txt", "b1\n");
    let b2 = env.commit("Change b2", "b2.txt", "b2\n");
    let out = env.try_spr(&["diff", "--all", "--one-pr", "-m", "unused"]);
    assert!(!out.success, "diff should refuse:\n{}", out.text);
    assert!(out.flat().contains("no description"), "{}", out.text);
    assert!(out.text.contains(&env.commit_id(&b1)[..7]), "{}", out.text);
    assert!(!out.text.contains("Created new"), "{}", out.text);
    assert_eq!(description(&env, &b2).trim(), "Change b2");
}

#[test]
fn steps_survive_github_rebase_after_a_stacked_merge() {
    // A, then B1, B2 and B3 as one PR stacked on A's. Landing A makes GitHub
    // replay the step commits onto main; after sync, a new change on top
    // is appended as one more step.
    let env = TestEnv::new();
    let a = env.commit("Change A", "a.txt", "a\n");
    let b: Vec<String> = ["b1", "b2", "b3"]
        .iter()
        .map(|name| env.commit(&format!("Change {name}"), &format!("{name}.txt"), "b\n"))
        .collect();
    env.spr(&["diff", "-r", &a, "-m", "A"]);
    env.spr(&[
        "diff",
        "-r",
        &format!("{}::{}", b[0], b[2]),
        "--one-pr",
        "-m",
        "unused",
    ]);
    let pb = env.pr_for(&b[2]);
    assert_eq!(env.pr(pb).base_ref, env.pr(env.pr_for(&a)).head_ref);

    env.spr(&["land", "-r", &a]);
    assert_eq!(env.pr(pb).base_ref, "main", "GitHub retargets the stack");
    assert_eq!(
        pr_commits(&env, pb),
        vec!["Change b1", "Change b2", "Change b3"],
        "GitHub replays the steps"
    );
    env.spr(&["sync", "-r", &b[2]]);
    let out = env.spr(&["diff", "--all", "-m", "after sync"]);
    assert!(out.text.contains("No update necessary"), "{}", out.text);

    let b4 = env.commit("Change b4", "b4.txt", "b\n");
    env.spr(&[
        "diff",
        "-r",
        &format!("{}::{b4}", b[0]),
        "--one-pr",
        "-m",
        "unused",
    ]);
    check_pr(
        &env,
        pb,
        "main",
        &["b1.txt", "b2.txt", "b3.txt", "b4.txt"],
        &b4,
    );
    assert_eq!(
        pr_commits(&env, pb),
        vec!["Change b1", "Change b2", "Change b3", "Change b4"]
    );
}

#[test]
fn one_pr_needs_a_range() {
    let env = TestEnv::new();
    let a = env.commit("Change A", "a.txt", "a\n");
    let out = env.try_spr(&["diff", "-r", &a, "--one-pr"]);
    assert!(!out.success, "diff should refuse:\n{}", out.text);
    assert!(out.flat().contains("--one-pr"), "{}", out.text);
}

#[test]
fn amending_a_middle_member_appends_one_commit() {
    let env = TestEnv::new();
    let b: Vec<String> = ["b1", "b2", "b3"]
        .iter()
        .map(|name| env.commit(&format!("Change {name}"), &format!("{name}.txt"), "b\n"))
        .collect();
    env.spr(&["diff", "--all", "--one-pr", "-m", "unused"]);
    let pr = env.pr_for(&b[2]);
    env.jj(&["edit", &b[1]]);
    std::fs::write(env.work.join("b2.txt"), "b2, revised\n").unwrap();
    env.jj(&["new", &b[2]]);

    let dry = env.spr(&["diff", "--all", "--dry-run"]);
    assert!(dry.flat().contains(&format!("#{pr}")), "{}", dry.text);
    assert_eq!(pr_commits(&env, pr).len(), 3, "a dry run pushes nothing");

    env.spr(&["diff", "--all", "-m", "review feedback"]);
    assert_eq!(
        pr_commits(&env, pr),
        vec!["Change b1", "Change b2", "Change b3", "review feedback"]
    );
    check_pr(&env, pr, "main", &["b1.txt", "b2.txt", "b3.txt"], &b[2]);
}

#[test]
fn adopting_a_stack_pushed_with_jj_git_push() {
    // A stack pushed with `jj git push --named`, opened and stacked on
    // GitHub by hand: X (x1, x2) <- Y (y1, y2, y3) <- Z (z1, z2). Adding
    // `Pull Request:` lines to every change makes it jj-spr's.
    let env = TestEnv::new();
    let groups: [&[&str]; 3] = [&["x1", "x2"], &["y1", "y2", "y3"], &["z1", "z2"]];
    let mut changes: Vec<Vec<String>> = Vec::new();
    let mut prs = Vec::new();
    let mut base = "main".to_string();
    for (i, group) in groups.iter().enumerate() {
        let members: Vec<String> = group
            .iter()
            .map(|name| env.commit(&format!("Change {name}"), &format!("{name}.txt"), "x\n"))
            .collect();
        let bookmark = format!("feature-{i}");
        let top = members.last().unwrap();
        env.jj(&["git", "push", "--named", &format!("{bookmark}={top}")]);
        let title = format!("Change {}", group.last().unwrap());
        prs.push(env.gh.state().open_pr_outside_spr(&bookmark, &base, &title));
        changes.push(members);
        base = bookmark;
    }
    env.gh.state().stack_outside_spr(&prs);
    let heads_before = heads(&env, &prs);

    // Adopt: a `Pull Request:` line on every change. Forget the local
    // bookmarks, which jj-spr does not move (the remote ones stay).
    for (members, pr) in changes.iter().zip(&prs) {
        for change in members {
            let text = format!(
                "{}\n\nPull Request: https://github.com/fake/repo/pull/{pr}",
                description(&env, change).trim()
            );
            env.jj(&["describe", "-r", change, "-m", &text]);
        }
    }
    env.jj(&["bookmark", "forget", "glob:feature-*"]);
    // Once jj-spr pushes on top of the old versions of these changes, jj
    // shows those again (they are in the PR branches' history), so each
    // change is divergent. Name the live version.
    let live = |change: &String| format!("change_id({change}) & mutable()");
    let tops: Vec<String> = changes.iter().map(|m| live(m.last().unwrap())).collect();

    // 1. Nothing changed: nothing is pushed, and the stack is kept.
    let out = env.spr(&["diff", "--all", "-m", "adopt"]);
    assert_eq!(
        out.text.matches("No update necessary").count(),
        3,
        "{}",
        out.text
    );
    assert_eq!(heads(&env, &prs), heads_before);
    assert_eq!(env.active_stacks(), vec![prs.clone()]);
    let out = env.spr(&["stack"]);
    assert!(
        out.text.contains("Everything matches GitHub"),
        "{}",
        out.text
    );

    // 2. A middle change of Y amended: Y gets one commit, Z merges it, and
    // every push fast-forwards the branch.
    env.jj(&["edit", &live(&changes[1][1])]);
    std::fs::write(env.work.join("y2.txt"), "y2, revised\n").unwrap();
    env.jj(&["new", &tops[2]]);
    env.spr(&["diff", "--all", "-m", "review"]);
    for (i, pr) in prs.iter().enumerate() {
        let base = if i == 0 {
            "main".to_string()
        } else {
            format!("feature-{}", i - 1)
        };
        assert_eq!(env.pr(*pr).base_ref, base);
        assert_eq!(pr_tree(&env, *pr), local_tree(&env, &tops[i]), "#{pr}");
        assert!(
            env.is_ancestor(&heads_before[i], &env.head_of(*pr)),
            "#{pr}"
        );
    }
    assert_eq!(pr_commits(&env, prs[1]).last().unwrap(), "review");
    assert_eq!(env.active_stacks(), vec![prs.clone()]);
    let divergent = env.jj_query(&[
        "log",
        "--no-graph",
        "-r",
        "divergent() & mutable()",
        "-T",
        "change_id ++ \"\\n\"",
    ]);
    assert_eq!(
        divergent.lines().count(),
        7,
        "every adopted change:\n{divergent}"
    );

    // 3. Rebased onto a newer main.
    env.push_to_main("m.txt", "m\n");
    env.jj(&["git", "fetch"]);
    env.jj(&["rebase", "-s", &live(&changes[0][0]), "-d", "main@origin"]);
    env.spr(&["diff", "--all", "-m", "rebase"]);
    for (i, pr) in prs.iter().enumerate() {
        assert_eq!(pr_tree(&env, *pr), local_tree(&env, &tops[i]), "#{pr}");
    }
    assert_eq!(env.active_stacks(), vec![prs.clone()]);
    let bookmarks = env.jj_query(&["bookmark", "list"]);
    assert!(!bookmarks.contains("conflict"), "{bookmarks}");

    // 4. New changes above, as one more Pull Request with `--one-pr`.
    env.jj(&["new", &tops[2]]);
    let w1 = env.commit("Change w1", "w1.txt", "w\n");
    let w2 = env.commit("Change w2", "w2.txt", "w\n");
    env.spr(&[
        "diff",
        "-r",
        &format!("{w1}::{w2}"),
        "--one-pr",
        "-m",
        "unused",
    ]);
    let pw = env.pr_for(&w2);
    check_pr(&env, pw, "feature-2", &["w1.txt", "w2.txt"], &w2);
    let mut stacked = prs.clone();
    stacked.push(pw);
    assert_eq!(env.active_stacks(), vec![stacked]);

    // 5. Land the bottom Pull Request and sync.
    env.spr(&["land", "-r", &tops[0]]);
    env.spr(&["sync", "-r", &w2]);
    for change in &changes[0] {
        assert!(
            !env.is_visible(&live(change)),
            "{change} should be abandoned"
        );
    }
    let out = env.spr(&["diff", "--all", "-m", "after landing"]);
    assert!(!out.text.contains("Created new"), "{}", out.text);
}

/// Nine changes in a line, a1 a2 a3 b1 b2 b3 c1 c2 c3, each titled
/// "Change <name>" and adding `<name>.txt`, in three groups of three.
fn nine_changes(env: &TestEnv) -> Vec<Vec<String>> {
    ["a", "b", "c"]
        .iter()
        .map(|group| {
            (1..=3)
                .map(|i| {
                    let name = format!("{group}{i}");
                    env.commit(
                        &format!("Change {name}"),
                        &format!("{name}.txt"),
                        &format!("{name}\n"),
                    )
                })
                .collect()
        })
        .collect()
}

/// A change titled "Change <name>" that adds `<name>.txt`, inserted below
/// `above`; returns its change ID.
fn insert_below(env: &TestEnv, above: &str, name: &str) -> String {
    env.jj(&[
        "new",
        "--insert-before",
        above,
        "-m",
        &format!("Change {name}"),
    ]);
    std::fs::write(env.work.join(format!("{name}.txt")), format!("{name}\n")).unwrap();
    env.change_id("@")
}

/// A `--tips` revset of the given changes.
fn tips(changes: &[&String]) -> String {
    changes
        .iter()
        .map(|c| c.as_str())
        .collect::<Vec<_>>()
        .join(" | ")
}

/// The top change of each group, as a `--tips` revset.
fn tops_of(groups: &[Vec<String>]) -> String {
    tips(&groups.iter().map(|g| &g[2]).collect::<Vec<_>>())
}

fn tips_open_a_stack(stacks_enabled: bool) {
    let env = TestEnv::with_stacks(stacks_enabled);
    if !stacks_enabled {
        env.jj(&["config", "set", "--repo", "spr.nativeStacks", "false"]);
    }
    let groups = nine_changes(&env);
    let above = env.commit("Change d1", "d1.txt", "d1\n");
    let before = env.request_count();

    env.spr(&["diff", "--tips", &tops_of(&groups), "-m", "unused"]);
    let prs: Vec<u64> = groups.iter().map(|g| env.pr_for(&g[2])).collect();
    let mut base = "main".to_string();
    for ((group, &pr), name) in groups.iter().zip(&prs).zip(["a", "b", "c"]) {
        let titles: Vec<String> = (1..=3).map(|i| format!("Change {name}{i}")).collect();
        for (change, title) in group.iter().zip(&titles) {
            assert_eq!(env.pr_for(change), pr, "{change} links #{pr}");
            assert!(
                description(&env, change).starts_with(&format!("{title}\n")),
                "{change} keeps its description"
            );
        }
        assert_eq!(env.pr(pr).title, titles[2]);
        let files: Vec<String> = (1..=3).map(|i| format!("{name}{i}.txt")).collect();
        let files: Vec<&str> = files.iter().map(String::as_str).collect();
        if !stacks_enabled {
            base = env.pr(pr).base_ref;
        }
        check_pr(&env, pr, &base, &files, &group[2]);
        assert_eq!(pr_commits(&env, pr), titles);
        base = env.pr(pr).head_ref;
    }
    assert!(
        !description(&env, &above).contains("Pull Request:"),
        "a change above the topmost tip is not submitted"
    );
    if stacks_enabled {
        assert_eq!(env.active_stacks(), vec![prs.clone()]);
        assert_eq!(env.requests_since(before, "POST", "/stacks").len(), 1);
    } else {
        assert!(env.stacks().is_empty());
    }

    let heads_before = heads(&env, &prs);
    let out = env.spr(&["diff", "--tips", &tops_of(&groups), "-m", "again"]);
    assert_eq!(
        out.text.matches("No update necessary").count(),
        3,
        "{}",
        out.text
    );
    let out = env.spr(&["diff", "-r", &groups[2][2], "--all", "-m", "again"]);
    assert_eq!(
        out.text.matches("No update necessary").count(),
        3,
        "{}",
        out.text
    );
    assert_eq!(heads(&env, &prs), heads_before);
}

#[test]
fn tips_open_a_stack_of_pull_requests_of_several_changes() {
    tips_open_a_stack(true);
}

#[test]
fn tips_open_a_stack_of_pull_requests_of_several_changes_legacy() {
    tips_open_a_stack(false);
}

#[test]
fn tips_add_new_changes_above_and_below_a_pull_request() {
    // a4 goes above a3 and b0 below b1. With a4 as the first tip, a4 joins
    // the Pull Request of a1..a3 at the top and b0 that of b1..b3 at the
    // bottom.
    let env = TestEnv::new();
    let groups = nine_changes(&env);
    env.spr(&["diff", "--tips", &tops_of(&groups), "-m", "unused"]);
    let prs: Vec<u64> = groups.iter().map(|g| env.pr_for(&g[2])).collect();
    let heads_before = heads(&env, &prs);

    let b0 = insert_below(&env, &groups[1][0], "b0");
    let a4 = insert_below(&env, &b0, "a4");
    env.jj(&["new", &groups[2][2]]);
    let tips = tips(&[&a4, &groups[1][2], &groups[2][2]]);
    env.spr(&["diff", "--tips", &tips, "-m", "add a4 and b0"]);

    assert_eq!(env.pr_for(&a4), prs[0]);
    assert_eq!(env.pr_for(&b0), prs[1]);
    assert_eq!(
        pr_commits(&env, prs[0]),
        vec!["Change a1", "Change a2", "Change a3", "Change a4"]
    );
    check_pr(
        &env,
        prs[0],
        "main",
        &["a1.txt", "a2.txt", "a3.txt", "a4.txt"],
        &a4,
    );
    check_pr(
        &env,
        prs[1],
        &env.pr(prs[0]).head_ref,
        &["b0.txt", "b1.txt", "b2.txt", "b3.txt"],
        &groups[1][2],
    );
    check_pr(
        &env,
        prs[2],
        &env.pr(prs[1]).head_ref,
        &["c1.txt", "c2.txt", "c3.txt"],
        &groups[2][2],
    );
    for (pr, head) in prs.iter().zip(&heads_before) {
        assert!(
            env.is_ancestor(head, &env.head_of(*pr)),
            "#{pr} fast-forwards"
        );
    }
    assert_eq!(env.active_stacks(), vec![prs.clone()]);

    let out = env.spr(&["diff", "--all", "-m", "again"]);
    assert_eq!(
        out.text.matches("No update necessary").count(),
        3,
        "{}",
        out.text
    );
}

#[test]
fn tips_refuse_to_split_or_join_pull_requests() {
    let env = TestEnv::new();
    let groups = nine_changes(&env);
    env.spr(&["diff", "--tips", &tops_of(&groups), "-m", "unused"]);
    let prs: Vec<u64> = groups.iter().map(|g| env.pr_for(&g[2])).collect();
    let before = heads(&env, &prs);

    // a2 is below the top of the first Pull Request.
    let below_top = tips(&[&groups[0][1], &groups[1][2], &groups[2][2]]);
    let out = env.try_spr(&["diff", "--tips", &below_top, "-m", "split"]);
    assert!(!out.success, "diff should refuse:\n{}", out.text);
    let text = out.flat();
    assert!(text.contains(&env.commit_id(&groups[0][1])[..7]), "{text}");
    assert!(text.contains(&format!("#{}", prs[0])), "{text}");
    assert!(text.contains("can't be split"), "{text}");

    // Without a3, a1..b3 would be one Pull Request.
    let missing = tips(&[&groups[1][2], &groups[2][2]]);
    let out = env.try_spr(&["diff", "--tips", &missing, "-m", "join"]);
    assert!(!out.success, "diff should refuse:\n{}", out.text);
    let text = out.flat();
    assert!(text.contains(&format!("#{}", prs[0])), "{text}");
    assert!(text.contains(&format!("#{}", prs[1])), "{text}");
    assert!(text.contains("top change of every Pull Request"), "{text}");
    assert!(!text.contains("--one-pr"), "{text}");

    assert_eq!(heads(&env, &prs), before, "nothing may be pushed");
}

#[test]
fn swap_two_pull_requests_of_two_changes_and_land() {
    // a1 a2 | b1 b2 becomes b1 b2 | a1 a2. Each Pull Request's commits are
    // then in the other's branch, and its update has to bring its changes
    // in commits of its own, or GitHub's rebase after the bottom one merges
    // leaves it with nothing to replay and closes it.
    let env = TestEnv::new();
    let groups: Vec<Vec<String>> = ["a", "b"]
        .iter()
        .map(|group| {
            (1..=2)
                .map(|i| {
                    let name = format!("{group}{i}");
                    env.commit(&format!("Change {name}"), &format!("{name}.txt"), "x\n")
                })
                .collect()
        })
        .collect();
    let tops = |groups: &[&Vec<String>]| tips(&groups.iter().map(|g| &g[1]).collect::<Vec<_>>());
    env.spr(&[
        "diff",
        "--tips",
        &tops(&[&groups[0], &groups[1]]),
        "-m",
        "unused",
    ]);
    let prs: Vec<u64> = groups.iter().map(|g| env.pr_for(&g[1])).collect();
    let main = env.branch("main").unwrap();

    let b = format!("{}::{}", groups[1][0], groups[1][1]);
    env.jj(&["rebase", "-r", &b, "--insert-before", &groups[0][0]]);
    env.jj(&["new", &groups[0][1]]);
    env.spr(&[
        "diff",
        "--tips",
        &tops(&[&groups[1], &groups[0]]),
        "-m",
        "swap",
    ]);
    check_pr(&env, prs[1], "main", &["b1.txt", "b2.txt"], &groups[1][1]);
    check_pr(
        &env,
        prs[0],
        &env.pr(prs[1]).head_ref,
        &["a1.txt", "a2.txt"],
        &groups[0][1],
    );
    assert_eq!(pr_commits(&env, prs[0]), vec!["Change a1", "Change a2"]);

    env.spr(&["land", "-r", &groups[0][1], "--yes"]);
    for &pr in &prs {
        let state = env.pr(pr);
        assert!(
            state.merged_at.is_some(),
            "PR #{pr} was not merged ({:?})",
            state.closed_reason
        );
    }
    let landed = env.remote_git(&[
        "log",
        "--reverse",
        "--format=%x00%s",
        "--name-only",
        &format!("{main}..main"),
    ]);
    let landed: Vec<String> = landed
        .split('\0')
        .map(|commit| commit.split_whitespace().collect::<Vec<_>>().join(" "))
        .filter(|commit| !commit.is_empty())
        .collect();
    assert_eq!(
        landed,
        vec!["Change b2 b1.txt b2.txt", "Change a2 a1.txt a2.txt"],
        "each Pull Request lands as one commit with its own files"
    );
}

#[test]
fn tips_need_a_line_of_changes() {
    let env = TestEnv::new();
    let a = env.commit("Change A", "a.txt", "a\n");
    env.jj(&["new", "trunk()"]);
    let x = env.commit("Change X", "x.txt", "x\n");

    let out = env.try_spr(&["diff", "--tips", &tips(&[&a, &x])]);
    assert!(!out.success, "diff should refuse:\n{}", out.text);
    assert!(out.flat().contains("in a line"), "{}", out.text);

    // The range up to a merge of A and X is not a line either.
    env.jj(&["new", &a, &x]);
    let merge = env.commit("Merge A and X", "m.txt", "m\n");
    let out = env.try_spr(&["diff", "--tips", &merge]);
    assert!(!out.success, "diff should refuse:\n{}", out.text);
    assert!(out.flat().contains("not a line of changes"), "{}", out.text);
    assert!(!out.flat().contains("--one-pr"), "{}", out.text);

    // A tip below the range cuts nothing.
    let out = env.try_spr(&["diff", "--tips", &format!("trunk() | {a}")]);
    assert!(!out.success, "diff should refuse:\n{}", out.text);
    assert!(out.flat().contains("not in the range"), "{}", out.text);

    assert!(env.gh.state().prs.is_empty(), "nothing may be opened");
}

#[test]
fn tips_conflict_with_other_selections() {
    let env = TestEnv::new();
    let a = env.commit("Change A", "a.txt", "a\n");
    for other in [&["-r", a.as_str()][..], &["--all"], &["--one-pr"]] {
        let mut args = vec!["diff", "--tips", a.as_str()];
        args.extend(other);
        let out = env.try_spr(&args);
        assert!(!out.success, "{args:?} should be refused:\n{}", out.text);
        assert!(out.flat().contains("cannot be used with"), "{}", out.text);
    }
}

#[test]
fn tips_dry_run_pushes_nothing() {
    let env = TestEnv::new();
    let groups = nine_changes(&env);
    let out = env.spr(&["diff", "--tips", &tops_of(&groups), "--dry-run"]);
    assert_eq!(out.text.matches("CREATE").count(), 3, "{}", out.text);
    assert!(env.gh.state().prs.is_empty(), "nothing may be opened");
    assert!(
        !env.gh
            .state()
            .branches()
            .keys()
            .any(|b| b.starts_with("spr/")),
        "nothing may be pushed"
    );
    assert!(!description(&env, &groups[0][2]).contains("Pull Request:"));
}
