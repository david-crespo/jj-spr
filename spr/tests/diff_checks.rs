//! End-to-end tests of what `jj spr diff` checks before it changes anything,
//! against a fake GitHub (see `common/fake_github.rs`). They need `jj` and
//! `git` on `PATH`. Set `SPR_TEST_VERBOSE=1` to print every command.

mod common;

use common::env::TestEnv;

fn heads(env: &TestEnv, prs: &[u64]) -> Vec<String> {
    prs.iter().map(|&n| env.head_of(n)).collect()
}

#[test]
fn diff_refuses_conflicted_changes() {
    // papertigers/jj-spr#11: a conflicted change was pushed as jj's
    // conflict files.
    let env = TestEnv::new();
    let changes = env.build_stack(&["a", "b"]);
    let prs = env.prs_for(&changes);
    env.push_to_main("a.txt", "someone else's a\n");
    env.jj(&["git", "fetch"]);
    env.jj(&["rebase", "-s", &changes[0], "-d", "main@origin"]);
    let before = heads(&env, &prs);
    let stacks = env.active_stacks();

    let out = env.try_spr(&["diff", "--all", "-m", "after rebase"]);
    assert!(!out.success, "diff should refuse:\n{}", out.text);
    assert!(out.flat().contains("conflicts"), "{}", out.text);
    assert!(out.text.contains(&changes[0][..8]), "{}", out.text);
    assert_eq!(heads(&env, &prs), before, "nothing may be pushed");
    assert_eq!(env.active_stacks(), stacks, "the stack must be kept");
}

/// A, B (with b.txt and b2.txt) and C, submitted, then B split in two so
/// both halves keep B's `Pull Request:` line.
fn stack_with_split_middle(env: &TestEnv) -> (Vec<String>, Vec<u64>) {
    let a = env.commit("Change A", "a.txt", "a\n");
    std::fs::write(env.work.join("b2.txt"), "b2\n").unwrap();
    let b = env.commit("Change B", "b.txt", "b\n");
    let c = env.commit("Change C", "c.txt", "c\n");
    env.spr(&["diff", "--all", "-m", "initial"]);
    let changes = vec![a, b, c];
    let prs = env.prs_for(&changes);
    // Accept the description unchanged, as `jj split` does in an editor.
    env.jj(&[
        "--config",
        "ui.editor=\"true\"",
        "split",
        "-r",
        &changes[1],
        "b2.txt",
    ]);
    (changes, prs)
}

#[test]
fn diff_refuses_two_changes_with_the_same_pull_request() {
    // papertigers/jj-spr#12: after `jj split`, both halves updated the same
    // PR, which ended up based on its own branch.
    let env = TestEnv::new();
    let (_, prs) = stack_with_split_middle(&env);
    let halves = env.jj_query(&[
        "log",
        "--no-graph",
        "-r",
        "description(substring:\"Change B\")",
        "-T",
        "change_id.short() ++ \"\\n\"",
    ]);
    let halves: Vec<&str> = halves.lines().collect();
    assert_eq!(halves.len(), 2, "{halves:?}");
    let before = heads(&env, &prs);
    let stacks = env.active_stacks();

    let out = env.try_spr(&["diff", "--all", "-m", "after split"]);
    assert!(!out.success, "diff should refuse:\n{}", out.text);
    assert!(out.flat().contains(&format!("#{}", prs[1])), "{}", out.text);
    for half in &halves {
        assert!(
            out.text.contains(&half[..8]),
            "should name {half}:\n{}",
            out.text
        );
    }
    assert_eq!(heads(&env, &prs), before, "nothing may be pushed");
    assert_eq!(env.active_stacks(), stacks, "the stack must be kept");
}

#[test]
fn diff_refuses_a_change_with_its_parents_pull_request() {
    // The same, submitting only the upper half: its parent (the lower half)
    // has the same Pull Request.
    let env = TestEnv::new();
    let (_, prs) = stack_with_split_middle(&env);
    let upper = env.change_id(
        "description(substring:\"Change B\") & children(description(substring:\"Change B\"))",
    );
    let before = heads(&env, &prs);

    let out = env.try_spr(&["diff", "-r", &upper, "-m", "after split"]);
    assert!(!out.success, "diff should refuse:\n{}", out.text);
    assert!(out.flat().contains(&format!("#{}", prs[1])), "{}", out.text);
    assert_eq!(heads(&env, &prs), before, "nothing may be pushed");
}

fn squash_two_submitted_changes_then_diff(stacks_enabled: bool) {
    let env = TestEnv::with_stacks(stacks_enabled);
    let changes = env.build_stack(&["a", "b", "c"]);
    let prs = env.prs_for(&changes);
    // Keep both descriptions, as `jj squash` does in an editor.
    env.jj(&[
        "--config",
        "ui.editor=\"true\"",
        "squash",
        "-r",
        &changes[1],
    ]);
    let description = env.jj_query(&["log", "--no-graph", "-r", &changes[0], "-T", "description"]);
    assert_eq!(
        description.matches("Pull Request:").count(),
        2,
        "{description}"
    );
    let before = heads(&env, &[prs[0], prs[2]]);
    let stacks = env.active_stacks();

    let out = env.try_spr(&["diff", "--all", "-m", "after squash"]);
    assert!(!out.success, "diff should refuse:\n{}", out.text);
    let text = out.flat();
    assert!(text.contains("more than one Pull Request"), "{}", out.text);
    assert!(text.contains(&format!("/pull/{}", prs[0])), "{}", out.text);
    assert!(text.contains(&format!("/pull/{}", prs[1])), "{}", out.text);
    assert!(!out.text.contains("Created new"), "{}", out.text);
    assert_eq!(
        heads(&env, &[prs[0], prs[2]]),
        before,
        "nothing may be pushed"
    );
    assert_eq!(env.active_stacks(), stacks, "the stack must be kept");
}

#[test]
fn diff_refuses_a_change_linking_two_pull_requests() {
    // papertigers/jj-spr#13: squashing two submitted changes combined their
    // `Pull Request:` lines, which could not be read, so diff opened a new
    // PR and left both old ones open.
    squash_two_submitted_changes_then_diff(true);
}

#[test]
fn diff_refuses_a_change_linking_two_pull_requests_without_stacks() {
    squash_two_submitted_changes_then_diff(false);
}

/// Submit A and B, then amend A, so a run that pushes before checking
/// would change A's Pull Request.
fn stack_with_amended_bottom(env: &TestEnv) -> (Vec<String>, Vec<u64>) {
    let changes = env.build_stack(&["a", "b"]);
    let prs = env.prs_for(&changes);
    std::fs::write(env.work.join("a.txt"), "a, amended\n").unwrap();
    env.jj(&["squash", "--into", &changes[0]]);
    (changes, prs)
}

/// `diff --all` refuses with `expect`, before pushing anything or touching
/// the stack.
fn assert_refused_before_changing_anything(env: &TestEnv, prs: &[u64], expect: &str) {
    let before = heads(env, prs);
    let stacks = env.active_stacks();
    let out = env.try_spr(&["diff", "--all", "-m", "update"]);
    assert!(!out.success, "diff should refuse:\n{}", out.text);
    assert!(out.flat().contains(expect), "{}", out.text);
    assert_eq!(heads(env, prs), before, "nothing may be pushed");
    assert_eq!(env.active_stacks(), stacks, "the stack must be kept");
}

#[test]
fn diff_checks_for_closed_pull_requests_before_pushing() {
    // papertigers/jj-spr#16: A was pushed before diff found B's PR closed.
    let env = TestEnv::new();
    let (_, prs) = stack_with_amended_bottom(&env);
    env.gh.state().close_on_github(prs[1]);
    assert_refused_before_changing_anything(&env, &prs, &format!("#{}", prs[1]));
}

#[test]
fn diff_checks_titles_before_pushing() {
    let env = TestEnv::new();
    let (_, prs) = stack_with_amended_bottom(&env);
    std::fs::write(env.work.join("c.txt"), "c\n").unwrap();
    env.jj(&["commit", "-m", ""]);
    assert_refused_before_changing_anything(&env, &prs, "title");
}

#[test]
fn diff_checks_reviewers_before_pushing() {
    let env = TestEnv::new();
    let (_, prs) = stack_with_amended_bottom(&env);
    std::fs::write(env.work.join("c.txt"), "c\n").unwrap();
    env.jj(&["commit", "-m", "Change C\n\nReviewers: nobody-at-all"]);
    assert_refused_before_changing_anything(&env, &prs, "nobody-at-all");
}

#[test]
fn diff_checks_cherry_picks_before_pushing() {
    // C changes a.txt, which main does not have, so it cannot be
    // cherry-picked there.
    let env = TestEnv::new();
    let (_, prs) = stack_with_amended_bottom(&env);
    std::fs::write(env.work.join("a.txt"), "a, from C\n").unwrap();
    env.jj(&["commit", "-m", "Change C\n\nCherry Pick: true"]);
    assert_refused_before_changing_anything(&env, &prs, "cannot be cherry-picked");
}

#[test]
fn diff_asks_for_the_message_before_dissolving_a_stack() {
    // Moving C below B means rebuilding the stack, and each updated PR
    // needs a message. The tests have no terminal, so the prompt fails as
    // leaving it empty (to abort) would.
    let env = TestEnv::new();
    let changes = env.build_stack(&["a", "b", "c"]);
    let prs = env.prs_for(&changes);
    env.jj(&["rebase", "-r", &changes[2], "--insert-before", &changes[1]]);
    let before = heads(&env, &prs);
    let stacks = env.active_stacks();

    let out = env.try_spr(&["diff", "--all"]);
    assert!(
        !out.success,
        "diff should stop at the prompt:\n{}",
        out.text
    );
    assert!(!out.text.contains("Dissolving"), "{}", out.text);
    assert_eq!(heads(&env, &prs), before, "nothing may be pushed");
    assert_eq!(env.active_stacks(), stacks, "the stack must be kept");
}

#[test]
fn diff_range_not_starting_on_main_stacks_on_the_parents_pr() {
    let env = TestEnv::new();
    let a = env.commit("Change A", "a.txt", "a\n");
    let b = env.commit("Change B", "b.txt", "b\n");
    env.spr(&["diff", "-r", &a, "-m", "A"]);
    let pa = env.pr_for(&a);

    env.spr(&["diff", "-r", &format!("{a}..{b}"), "-m", "range"]);
    let pb = env.pr_for(&b);
    assert_eq!(env.pr(pb).base_ref, env.pr(pa).head_ref);
    assert_eq!(env.pr_files(pb), vec!["b.txt"]);
}

#[test]
fn diff_range_not_starting_on_main_without_native_stacks() {
    // Without native stacks, B's PR gets a base branch with A's changes,
    // as it does when B is submitted on its own.
    let env = TestEnv::new();
    let a = env.commit("Change A", "a.txt", "a\n");
    let b = env.commit("Change B", "b.txt", "b\n");

    env.spr(&[
        "diff",
        "-r",
        &format!("{a}..{b}"),
        "--no-native-stack",
        "-m",
        "range",
    ]);
    let pb = env.pr_for(&b);
    assert_ne!(env.pr(pb).base_ref, "main");
    assert_eq!(env.pr_files(pb), vec!["b.txt"]);
}

#[test]
fn diff_range_above_a_stacked_pull_request_keeps_the_stack() {
    // `-r B::C` in the stack A, B, C: B's base (A's branch) does not change,
    // so the stack is kept as it is.
    let env = TestEnv::new();
    let changes = env.build_stack(&["a", "b", "c"]);
    let prs = env.prs_for(&changes);
    let stacks = env.active_stacks();
    env.jj(&["edit", &changes[1]]);
    std::fs::write(env.work.join("b.txt"), "b, revised\n").unwrap();
    env.jj(&["new", &changes[2]]);

    let out = env.spr(&[
        "diff",
        "-r",
        &format!("{}::{}", changes[1], changes[2]),
        "-m",
        "range",
    ]);
    assert!(!out.text.contains("Dissolving"), "{}", out.text);
    assert_eq!(env.pr(prs[1]).base_ref, env.pr(prs[0]).head_ref);
    assert_eq!(env.pr_files(prs[1]), vec!["b.txt"]);
    assert_eq!(env.active_stacks(), stacks, "the stack must be kept");
}

/// B and C, submitted as a stack, with a new change A inserted below B.
/// `-r B::C` has to stack B on A's Pull Request, which it does not update.
fn range_above_a_new_parent(env: &TestEnv) -> (String, Vec<String>, Vec<u64>) {
    let changes = env.build_stack(&["b", "c"]);
    let prs = env.prs_for(&changes);
    env.jj(&["new", "main@origin"]);
    let a = env.commit("Change A", "a.txt", "a\n");
    env.jj(&["rebase", "-s", &changes[0], "-d", &a]);
    (a, changes, prs)
}

/// `diff -r B::C` refuses with `expect`, before pushing anything or
/// dissolving the stack it could not rebuild.
fn assert_range_refused_keeping_the_stack(
    env: &TestEnv,
    changes: &[String],
    prs: &[u64],
    expect: &str,
) {
    let before = heads(env, prs);
    let stacks = env.active_stacks();
    let range = format!("{}::{}", changes[0], changes[1]);
    let out = env.try_spr(&["diff", "-r", &range, "-m", "move"]);
    assert!(!out.success, "diff should refuse:\n{}", out.text);
    assert!(out.flat().contains(expect), "{}", out.text);
    assert_eq!(heads(env, prs), before, "nothing may be pushed");
    assert_eq!(env.active_stacks(), stacks, "the stack must be kept");
}

#[test]
fn diff_range_above_an_unsubmitted_parent_keeps_the_stack() {
    // papertigers/jj-spr#14: a range that does not start on main opened or
    // updated its bottom Pull Request against main with the parent's
    // changes in it.
    let env = TestEnv::new();
    let (_, changes, prs) = range_above_a_new_parent(&env);
    assert_range_refused_keeping_the_stack(&env, &changes, &prs, "has no Pull Request yet");
}

#[test]
fn diff_range_above_a_closed_parent_keeps_the_stack() {
    let env = TestEnv::new();
    let (a, changes, prs) = range_above_a_new_parent(&env);
    env.spr(&["diff", "-r", &a, "-m", "A"]);
    let pa = env.pr_for(&a);
    env.gh.state().close_on_github(pa);
    assert_range_refused_keeping_the_stack(&env, &changes, &prs, &format!("#{pa} is closed"));
}

#[test]
fn diff_of_a_change_whose_parent_merged_points_at_sync() {
    let env = TestEnv::new();
    let changes = env.build_stack(&["a", "b", "c"]);
    let prs = env.prs_for(&changes);
    env.gh.state().merge_on_github(prs[0]);
    let before = heads(&env, &prs[1..]);

    let out = env.try_spr(&["diff", "-r", &changes[1], "-m", "before sync"]);
    assert!(!out.success, "diff should refuse:\n{}", out.text);
    let text = out.flat();
    assert!(
        text.contains(&format!("#{} has merged", prs[0])),
        "{}",
        out.text
    );
    assert!(text.contains("jj spr sync"), "{}", out.text);
    assert_eq!(heads(&env, &prs[1..]), before, "nothing may be pushed");
}

#[test]
fn diff_of_one_stacked_change_refuses_a_base_change_before_pushing() {
    // papertigers/jj-spr#22: single-change mode pushed B, then failed to
    // change its base, which GitHub refuses for a PR in a stack.
    let env = TestEnv::new();
    let changes = env.build_stack(&["a", "b", "c"]);
    let prs = env.prs_for(&changes);
    env.jj(&["rebase", "-r", &changes[1], "-d", "main@origin"]);
    let before = heads(&env, &prs);
    let stacks = env.active_stacks();

    let out = env.try_spr(&["diff", "-r", &changes[1], "-m", "move B"]);
    assert!(!out.success, "diff should refuse:\n{}", out.text);
    let text = out.flat();
    assert!(text.contains("jj spr diff --all"), "{}", out.text);
    let a_branch = env.pr(prs[0]).head_ref;
    assert!(
        text.contains(&format!("#{} from {a_branch} to main", prs[1])),
        "{}",
        out.text
    );
    assert_eq!(heads(&env, &prs), before, "nothing may be pushed");
    assert_eq!(env.active_stacks(), stacks, "the stack must be kept");
}

#[test]
fn diff_of_the_last_open_member_of_a_stack_moves_it() {
    // After A merged, B is the only open PR in its stack, so moving B onto
    // another PR dissolves the old stack and starts a new one.
    let env = TestEnv::new();
    let changes = env.build_stack(&["a", "b"]);
    let prs = env.prs_for(&changes);
    env.gh.state().merge_on_github(prs[0]);
    env.spr(&["sync"]);
    assert_eq!(env.active_stacks(), vec![vec![prs[1]]]);
    env.jj(&["new", "main@origin"]);
    let x = env.commit("Change X", "x.txt", "x\n");
    env.spr(&["diff", "-r", &x, "-m", "X"]);
    let px = env.pr_for(&x);
    env.jj(&["rebase", "-s", &changes[1], "-d", &x]);

    env.spr(&["diff", "-r", &changes[1], "-m", "move B"]);
    assert_eq!(env.pr(prs[1]).base_ref, env.pr(px).head_ref);
    assert_eq!(env.pr_files(prs[1]), vec!["b.txt"]);
    assert_eq!(env.active_stacks(), vec![vec![px, prs[1]]]);
}

#[test]
fn diff_avoids_a_branch_name_taken_on_github() {
    let env = TestEnv::new();
    let change = env.commit("Change A", "a.txt", "a\n");
    // Pushed by someone else, and never fetched here.
    let theirs = env.remote_git(&["commit-tree", "main^{tree}", "-m", "Someone else's work"]);
    env.remote_git(&["update-ref", "refs/heads/spr/change-a", &theirs]);
    env.spr(&["diff", "-r", &change]);
    assert_eq!(env.pr(env.pr_for(&change)).head_ref, "spr/change-a-1");
    assert_eq!(env.branch("spr/change-a"), Some(theirs));
}

#[test]
fn the_branch_prefix_flag_replaces_the_setting() {
    let env = TestEnv::new();
    let a = env.commit("Change A", "a.txt", "a\n");

    env.spr(&["--branch-prefix", "flag/", "diff", "-r", &a]);
    assert_eq!(env.pr(env.pr_for(&a)).head_ref, "flag/change-a");
}
