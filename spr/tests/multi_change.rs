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
