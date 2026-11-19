//! End-to-end tests of `jj spr diff`, `land` and `close` with native GitHub
//! stacks, against a fake GitHub (see `common/fake_github.rs`).
//!
//! They need `jj` and `git` on `PATH`, like the other integration tests. Set
//! `SPR_TEST_VERBOSE=1` to print every command.

mod common;

use common::env::TestEnv;

fn strings(ids: &[&String]) -> Vec<String> {
    ids.iter().map(|s| s.to_string()).collect()
}

#[test]
fn create_stack() {
    let env = TestEnv::new();
    let changes = env.build_stack(&["a", "b", "c"]);
    let prs = env.check_chain(&changes);
    assert_eq!(prs, vec![1, 2, 3]);
    assert!(
        !env.gh
            .state()
            .branches()
            .keys()
            .any(|b| b.starts_with("spr/main.")),
        "diff should not create spr/main.* base branches"
    );
    assert_eq!(
        env.requests_since(0, "POST", "/stacks").len(),
        1,
        "one stack creation"
    );
}

#[test]
fn rerun_is_noop() {
    let env = TestEnv::new();
    let changes = env.build_stack(&["a", "b", "c"]);
    let heads = env.gh.state().branches();
    let before = env.request_count();
    let out = env.spr(&["diff", "--all", "-m", "again"]);
    assert_eq!(env.gh.state().branches(), heads, "rerun changed branches");
    assert!(out.text.contains("No update necessary"), "{}", out.text);
    for method in ["POST", "PATCH", "PUT"] {
        let changes: Vec<_> = env
            .requests_since(before, method, "/repos/")
            .into_iter()
            .collect();
        assert!(changes.is_empty(), "rerun made changes: {changes:?}");
    }
    env.check_chain(&changes);
}

#[test]
fn amend_middle() {
    let env = TestEnv::new();
    let changes = env.build_stack(&["a", "b", "c"]);
    let prs = env.prs_for(&changes);
    let old: Vec<String> = prs.iter().map(|&n| env.head_of(n)).collect();

    env.jj(&["edit", &changes[1]]);
    std::fs::write(env.work.join("b.txt"), "b, revised\n").unwrap();
    env.jj(&["new", &changes[2]]);
    env.spr(&["diff", "--all", "-m", "review feedback"]);

    env.check_chain(&changes);
    assert_eq!(
        env.head_of(prs[0]),
        old[0],
        "the bottom PR should not change"
    );
    for (i, &n) in prs.iter().enumerate().skip(1) {
        let new = env.head_of(n);
        assert_ne!(new, old[i], "PR #{n} was not updated");
        assert!(env.is_ancestor(&old[i], &new), "PR #{n} lost its history");
    }
    let message = env.remote_git(&["log", "-1", "--format=%s", &env.head_of(prs[1])]);
    assert_eq!(message, "review feedback");
}

#[test]
fn rebase_on_main() {
    let env = TestEnv::new();
    let changes = env.build_stack(&["a", "b", "c"]);
    let prs = env.prs_for(&changes);
    let old: Vec<String> = prs.iter().map(|&n| env.head_of(n)).collect();

    // Someone else lands work on main.
    env.git_in(&env.seed, &["pull", "-q", "origin", "main"]);
    std::fs::write(env.seed.join("other.txt"), "other\n").unwrap();
    env.git_in(&env.seed, &["add", "other.txt"]);
    env.git_in(&env.seed, &["commit", "-q", "-m", "Other work"]);
    env.git_in(&env.seed, &["push", "-q", "origin", "main"]);

    env.jj(&["git", "fetch"]);
    env.jj(&["rebase", "-s", &changes[0], "-d", "main@origin"]);
    let before = env.request_count();
    env.spr(&["diff", "--all", "-m", "rebase on main"]);

    env.check_chain(&changes);
    let main = env.branch("main").unwrap();
    for (i, &n) in prs.iter().enumerate() {
        let new = env.head_of(n);
        assert_ne!(new, old[i], "PR #{n} was not updated");
        assert!(env.is_ancestor(&old[i], &new), "PR #{n} was rewritten");
        assert!(
            env.is_ancestor(&main, &new),
            "PR #{n} does not include main"
        );
        let parents = env.remote_git(&["log", "-1", "--format=%P", &new]);
        assert_eq!(
            parents.split(' ').next(),
            Some(old[i].as_str()),
            "PR #{n}'s first parent"
        );
    }
    assert_eq!(env.pr_files(prs[0]), vec!["a.txt"]);
    assert!(
        env.requests_since(before, "POST", "/stacks").is_empty(),
        "a rebase should not touch the stack"
    );
}

#[test]
fn append_to_stack() {
    let env = TestEnv::new();
    let mut changes = env.build_stack(&["a", "b", "c"]);
    changes.push(env.commit("Change D", "d.txt", "d\n"));
    let before = env.request_count();
    env.spr(&["diff", "--all", "-m", "add d"]);
    let prs = env.check_chain(&changes);
    assert_eq!(prs.len(), 4);
    assert_eq!(env.stacks().len(), 1, "{:?}", env.stacks());
    assert_eq!(
        env.requests_since(before, "POST", "/add").len(),
        1,
        "append to the stack"
    );
}

#[test]
fn single_change_mode() {
    let env = TestEnv::new();
    let mut changes = env.build_stack(&["a", "b", "c"]);
    changes.push(env.commit("Change D", "d.txt", "d\n"));
    env.spr(&["diff", "-r", "@-"]);
    env.check_chain(&changes);
}

#[test]
fn single_change_needs_parent_pr() {
    let env = TestEnv::new();
    env.commit("Change A", "a.txt", "a\n");
    env.commit("Change B", "b.txt", "b\n");
    let out = env.try_spr(&["diff", "-r", "@-"]);
    assert!(!out.success);
    assert!(
        out.flat().contains("has no Pull Request yet"),
        "{}",
        out.text
    );
    assert!(
        env.gh.state().prs.is_empty(),
        "no PR should have been created"
    );
}

#[test]
fn reorder() {
    let env = TestEnv::new();
    let changes = env.build_stack(&["a", "b", "c", "d"]);
    let old = env.prs_for(&changes);
    // Move D below C: a, b, d, c
    env.jj(&["rebase", "-r", &changes[3], "--insert-before", &changes[2]]);
    let out = env.spr(&["diff", "--all", "-m", "reorder"]);
    assert!(out.text.contains("Dissolving GitHub stack"), "{}", out.text);
    let prs = env.check_chain(&strings(&[
        &changes[0],
        &changes[1],
        &changes[3],
        &changes[2],
    ]));
    assert_eq!(
        prs,
        vec![old[0], old[1], old[3], old[2]],
        "PRs keep their numbers"
    );
}

/// Submit a, b, and c, reorder them locally to `order` (indexes into a, b,
/// and c, bottom first), submit again, and land the whole stack. Each Pull
/// Request has to reach main as its own commit, titled for it, with only its
/// own file.
fn reorder_and_land(order: [usize; 3]) {
    let env = TestEnv::new();
    let changes = env.build_stack(&["a", "b", "c"]);
    let prs = env.prs_for(&changes);
    let main = env.branch("main").unwrap();
    for pair in order.windows(2) {
        env.jj(&[
            "rebase",
            "-r",
            &changes[pair[1]],
            "--insert-after",
            &changes[pair[0]],
        ]);
    }
    env.jj(&["new", &changes[order[2]]]);
    env.spr(&["diff", "--all", "-m", "reorder"]);
    let reordered: Vec<String> = order.iter().map(|&i| changes[i].clone()).collect();
    env.check_chain(&reordered);

    env.spr(&["land", "-r", &reordered[2], "--yes"]);
    for &i in &order {
        let pr = env.pr(prs[i]);
        assert!(
            pr.merged_at.is_some(),
            "PR #{} was not merged ({:?})",
            prs[i],
            pr.closed_reason
        );
    }
    let landed = ["Change A: a.txt", "Change B: b.txt", "Change C: c.txt"];
    let expected: Vec<&str> = order.iter().map(|&i| landed[i]).collect();
    assert_eq!(env.landed_since(&main), expected);
}

#[test]
fn reorder_two_adjacent_pull_requests_and_land() {
    // B moves above C. B's original commit is then in C's branch, so B's
    // update has to bring B's change in a commit of its own, or GitHub's
    // rebase after A merges leaves B with no commits and closes it.
    reorder_and_land([0, 2, 1]);
}

#[test]
fn move_the_top_pull_request_to_the_bottom_and_land() {
    reorder_and_land([2, 0, 1]);
}

#[test]
fn insert_in_middle() {
    let env = TestEnv::new();
    let changes = env.build_stack(&["a", "b", "c"]);
    let old = env.prs_for(&changes);
    let old_heads: Vec<String> = old.iter().map(|&n| env.head_of(n)).collect();

    // Insert X between B and C.
    env.jj(&["new", "--insert-after", &changes[1], "-m", "Change X"]);
    std::fs::write(env.work.join("x.txt"), "x\n").unwrap();
    let inserted = env.change_id("@");
    env.jj(&["new", &changes[2]]);
    let out = env.spr(&["diff", "--all", "-m", "insert x"]);
    assert!(out.text.contains("Dissolving GitHub stack"), "{}", out.text);

    let prs = env.check_chain(&strings(&[
        &changes[0],
        &changes[1],
        &inserted,
        &changes[2],
    ]));
    assert_eq!(
        vec![prs[0], prs[1], prs[3]],
        old,
        "existing PRs keep their numbers"
    );
    assert_eq!(env.head_of(old[0]), old_heads[0], "PR A changed");
    assert_eq!(env.head_of(old[1]), old_heads[1], "PR B changed");
    assert!(
        env.is_ancestor(&old_heads[2], &env.head_of(old[2])),
        "PR C was rewritten"
    );
    assert_eq!(env.active_stacks().len(), 1, "{:?}", env.stacks());
}

#[test]
fn insert_single_change() {
    let env = TestEnv::new();
    let changes = env.build_stack(&["a", "b", "c"]);
    let old = env.prs_for(&changes);
    env.jj(&["new", "--insert-after", &changes[1], "-m", "Change X"]);
    std::fs::write(env.work.join("x.txt"), "x\n").unwrap();
    let inserted = env.change_id("@");
    env.jj(&["new", &changes[2]]);

    // Submitting only X cannot retarget C, so it must say so and change nothing else.
    let out = env.spr(&["diff", "-r", &inserted]);
    let x = env.pr_for(&inserted);
    assert_eq!(
        env.pr(x).base_ref,
        env.pr(old[1]).head_ref,
        "X should target B"
    );
    assert_eq!(
        env.pr(old[2]).base_ref,
        env.pr(old[1]).head_ref,
        "C should be untouched"
    );
    assert!(out.flat().contains("jj spr diff --all"), "{}", out.text);
    assert_eq!(
        env.active_stacks(),
        vec![old.clone()],
        "stack should be unchanged"
    );

    // Following the advice fixes it.
    env.spr(&["diff", "--all", "-m", "restack"]);
    env.check_chain(&strings(&[
        &changes[0],
        &changes[1],
        &inserted,
        &changes[2],
    ]));
}

#[test]
fn migrate_legacy_stack() {
    let env = TestEnv::new();
    let changes = vec![
        env.commit("Change A", "a.txt", "a\n"),
        env.commit("Change B", "b.txt", "b\n"),
    ];
    // Opened by an older jj-spr: B targets a base branch of its own.
    let prs = env.open_legacy_pull_requests(&changes);
    let legacy_base = env.pr(prs[1]).base_ref;
    env.spr(&["diff", "--all", "-m", "retarget"]);
    assert_eq!(env.check_chain(&changes), prs);
    assert!(
        env.branch(&legacy_base).is_some(),
        "the old base branch is left for `jj spr cleanup`"
    );
}

#[test]
fn land_bottom() {
    let env = TestEnv::new();
    let changes = env.build_stack(&["a", "b", "c"]);
    let prs = env.prs_for(&changes);
    let bottom_branch = env.pr(prs[0]).head_ref;

    env.spr(&["land", "-r", &changes[0]]);
    assert!(
        env.pr(prs[0]).merged_at.is_some(),
        "bottom PR was not merged"
    );
    assert_eq!(
        env.requests_since(0, "PUT", "merge-async").len(),
        1,
        "a stack merge"
    );
    for &n in &prs[1..] {
        let pr = env.pr(n);
        assert!(pr.is_open(), "PR #{n} was closed: {:?}", pr.closed_reason);
    }
    assert_eq!(
        env.pr(prs[1]).base_ref,
        "main",
        "next PR should now target main"
    );
    assert!(
        env.branch(&bottom_branch).is_none(),
        "landed branch should be deleted"
    );

    // Bring the local stack up to date and resubmit.
    env.jj(&["git", "fetch"]);
    env.jj(&["rebase", "-s", &changes[1], "-d", "main@origin"]);
    env.jj(&["abandon", &changes[0]]);
    env.spr(&["diff", "--all", "-m", "after landing"]);
    env.check_chain(&changes[1..]);
    assert_eq!(env.pr_files(prs[1]), vec!["b.txt"]);
}

/// The messages of the commits on main after `since`, oldest first.
fn messages_on_main_since(env: &TestEnv, since: &str) -> Vec<String> {
    env.remote_git(&[
        "log",
        "--reverse",
        "--format=%B%x00",
        &format!("{since}..main"),
    ])
    .split('\0')
    .map(str::trim)
    .filter(|m| !m.is_empty())
    .map(str::to_string)
    .collect()
}

/// The squash message jj-spr gives Pull Request `number` titled `title`.
fn landed_message(number: u64, title: &str) -> String {
    let url = format!("https://github.com/fake/repo/pull/{number}");
    format!("{title}\n\nReviewers: \n\nPull Request: {url}")
}

#[test]
fn land_top_gives_every_pull_request_its_message() {
    let env = TestEnv::new();
    let changes = env.build_stack(&["a", "b", "c"]);
    let prs = env.prs_for(&changes);
    let main = env.branch("main").unwrap();

    env.spr(&["land", "-r", &changes[2], "--yes"]);
    assert_eq!(
        messages_on_main_since(&env, &main),
        vec![
            landed_message(prs[0], "Change A"),
            landed_message(prs[1], "Change B"),
            landed_message(prs[2], "Change C"),
        ]
    );
}

#[test]
fn land_middle_needs_confirmation() {
    let env = TestEnv::new();
    let changes = env.build_stack(&["a", "b", "c"]);
    let prs = env.prs_for(&changes);
    let out = env.try_spr(&["land", "-r", &changes[1]]);
    assert!(!out.success, "land should ask first:\n{}", out.text);
    let text = out.flat();
    assert!(
        text.contains(&format!("merges #{}, #{}", prs[0], prs[1])),
        "{}",
        out.text
    );
    assert!(text.contains("--yes"), "{}", out.text);
    assert!(
        env.gh.state().prs.values().all(|pr| pr.merged_at.is_none()),
        "nothing should merge"
    );
}

#[test]
fn land_middle_merges_stack_below() {
    let env = TestEnv::new();
    let changes = env.build_stack(&["a", "b", "c", "d"]);
    let prs = env.prs_for(&changes);
    let merged_branches: Vec<String> = prs[..3].iter().map(|&n| env.pr(n).head_ref).collect();

    env.spr(&["land", "-r", &changes[2], "--yes"]);
    for &n in &prs[..3] {
        assert!(env.pr(n).merged_at.is_some(), "PR #{n} should be merged");
    }
    assert_eq!(
        env.requests_since(0, "PUT", "merge-async").len(),
        3,
        "one stack merge per Pull Request"
    );
    let top = env.pr(prs[3]);
    assert!(
        top.is_open(),
        "PR #{} was closed: {:?}",
        prs[3],
        top.closed_reason
    );
    assert_eq!(
        top.base_ref, "main",
        "the rest of the stack now targets main"
    );
    for branch in &merged_branches {
        assert!(env.branch(branch).is_none(), "{branch} should be deleted");
    }

    env.spr(&["sync"]);
    assert!(changes[..3].iter().all(|c| !env.is_visible(c)));
    env.spr(&["diff", "--all", "-m", "after landing"]);
    assert_eq!(env.pr_files(prs[3]), vec!["d.txt"]);
}

#[test]
fn land_middle_checks_stack_below() {
    let env = TestEnv::new();
    let changes = env.build_stack(&["a", "b"]);
    let prs = env.prs_for(&changes);
    env.gh.state().prs.get_mut(&prs[0]).unwrap().draft = true;

    let out = env.try_spr(&["land", "-r", &changes[1], "--yes"]);
    assert!(!out.success, "land should refuse:\n{}", out.text);
    assert!(
        out.flat().contains(&format!("#{} is a draft", prs[0])),
        "{}",
        out.text
    );
    assert!(
        env.gh.state().prs.values().all(|pr| pr.merged_at.is_none()),
        "nothing should merge"
    );
}

#[test]
fn land_refuses_draft() {
    for stacks_enabled in [true, false] {
        let env = TestEnv::with_stacks(stacks_enabled);
        let changes = vec![
            env.commit("Change A", "a.txt", "a\n"),
            env.commit("Change B", "b.txt", "b\n"),
        ];
        env.spr(&["diff", "--all", "--draft"]);
        let prs = env.prs_for(&changes);
        let requests = env.request_count();

        let out = env.try_spr(&["land", "-r", &changes[0]]);
        assert!(!out.success, "land should refuse a draft:\n{}", out.text);
        assert!(
            out.flat().contains(&format!("#{} is a draft", prs[0])),
            "{}",
            out.text
        );
        assert!(
            env.requests_since(requests, "PUT", "merge").is_empty(),
            "no merge should be attempted"
        );
        assert!(env.pr(prs[0]).is_open());
    }
}

#[test]
fn close_keeps_dependents() {
    let env = TestEnv::new();
    let changes = env.build_stack(&["a", "b", "c"]);
    let prs = env.prs_for(&changes);
    let middle_branch = env.pr(prs[1]).head_ref;
    // GitHub's rules for closing stacked PRs are not modelled.
    env.gh.state().stacks.clear();

    let out = env.spr(&["close", "-r", &changes[1]]);
    assert!(
        out.flat()
            .contains(&format!("because #{} still targets it", prs[2])),
        "{}",
        out.text
    );
    assert!(!env.pr(prs[1]).is_open(), "middle PR should be closed");
    assert!(
        env.branch(&middle_branch).is_some(),
        "branch of the closed PR was deleted"
    );
    let dependent = env.pr(prs[2]);
    assert!(
        dependent.is_open(),
        "dependent PR was closed: {:?}",
        dependent.closed_reason
    );
}

#[test]
fn close_the_top_pull_request_and_open_a_new_one() {
    // GitHub keeps the closed Pull Request at the top of the stack, and
    // chains a Pull Request added to the stack from it, so the stack has to
    // be rebuilt.
    let env = TestEnv::new();
    let changes = env.build_stack(&["a", "b", "c"]);
    let prs = env.prs_for(&changes);
    let old: Vec<u64> = env.stacks().into_keys().collect();
    env.spr(&["close", "-r", &changes[2]]);
    assert!(!env.pr(prs[2]).is_open());

    let out = env.spr(&["diff", "--all", "-m", "reopen C"]);
    assert!(!out.text.contains("failed"), "{}", out.text);
    let now = env.check_chain(&changes);
    assert_eq!(now[..2], prs[..2]);
    assert!(!prs.contains(&now[2]), "C gets a new Pull Request");
    assert_eq!(env.active_stacks(), vec![now]);
    assert!(
        env.stacks().keys().all(|stack| !old.contains(stack)),
        "the old stack is dissolved: {:?}",
        env.stacks()
    );
}

#[test]
fn stacks_disabled() {
    let env = TestEnv::with_stacks(false);
    let changes = env.build_stack(&["a", "b", "c"]);
    let prs = env.prs_for(&changes);
    for i in 1..prs.len() {
        assert_eq!(
            env.pr(prs[i]).base_ref,
            env.pr(prs[i - 1]).head_ref,
            "PRs not chained"
        );
        assert_eq!(
            env.pr_files(prs[i]),
            vec![format!("{}.txt", ["a", "b", "c"][i])]
        );
    }
    assert!(env.stacks().is_empty(), "no stacks when disabled");
}

#[test]
fn land_refuses_a_chained_pull_request_without_a_stack() {
    let env = TestEnv::with_stacks(false);
    let changes = env.build_stack(&["a", "b"]);
    let prs = env.prs_for(&changes);
    let bases: Vec<String> = prs.iter().map(|&n| env.pr(n).base_ref).collect();

    let out = env.try_spr(&["land", "-r", &changes[1]]);
    assert!(!out.success, "land should refuse:\n{}", out.text);
    assert!(
        out.flat().contains(&format!("Land #{} first", prs[0])),
        "{}",
        out.text
    );
    assert!(
        env.gh.state().prs.values().all(|pr| pr.merged_at.is_none()),
        "nothing should merge"
    );
    let after: Vec<String> = prs.iter().map(|&n| env.pr(n).base_ref).collect();
    assert_eq!(after, bases, "bases must be unchanged");

    // Following the advice lands each as its own commit.
    let main = env.branch("main").unwrap();
    let out = env.spr(&["land", "-r", &changes[0]]);
    assert!(
        out.flat()
            .contains(&format!("#{} now targets main", prs[1])),
        "{}",
        out.text
    );
    env.spr(&["sync"]);
    env.spr(&["land", "-r", &changes[1]]);
    assert_eq!(
        env.landed_since(&main),
        vec!["Change A: a.txt", "Change B: b.txt"]
    );
}

#[test]
fn land_refuses_a_pull_request_on_a_legacy_base_branch() {
    // Earlier versions of jj-spr based a Pull Request on a branch of its
    // own. `jj spr diff` retargets it; `land` does not.
    let env = TestEnv::new();
    let changes = vec![
        env.commit("Change A", "a.txt", "a\n"),
        env.commit("Change B", "b.txt", "b\n"),
    ];
    let prs = env.open_legacy_pull_requests(&changes);
    let base = env.pr(prs[1]).base_ref;
    assert_eq!(base, "spr/main.change-b");

    env.spr(&["land", "-r", &changes[0]]);
    let out = env.try_spr(&["land", "-r", &changes[1]]);
    assert!(!out.success, "land should refuse:\n{}", out.text);
    assert!(
        out.flat()
            .contains(&format!("#{} targets {base}, not main", prs[1])),
        "{}",
        out.text
    );
    assert!(out.flat().contains("Run `jj spr diff`"), "{}", out.text);
    assert_eq!(env.pr(prs[1]).base_ref, base, "land must not retarget");
    assert!(env.pr(prs[1]).merged_at.is_none(), "B must not merge");

    env.spr(&["sync"]);
    env.spr(&["diff", "--all", "-m", "retarget"]);
    assert_eq!(env.pr(prs[1]).base_ref, "main");
    assert_eq!(env.pr_files(prs[1]), vec!["b.txt"]);
    env.spr(&["land", "-r", &changes[1]]);
    assert!(env.pr(prs[1]).merged_at.is_some(), "B should be merged");

    // The base branch is left over, and `cleanup` removes it.
    assert!(env.branch(&base).is_some());
    env.jj(&["git", "fetch"]);
    env.spr(&["cleanup", "--confirm"]);
    assert!(env.branch(&base).is_none(), "cleanup should delete {base}");
}

#[test]
fn github_rebase_after_merge_keeps_edits_in_merge_commits() {
    // Amending a change and the one below it in one run gives the upper PR
    // an update commit that is a merge carrying its own edit. GitHub's
    // post-merge rebase drops merge commits, so check what survives.
    let env = TestEnv::new();
    let changes = env.build_stack(&["a", "b", "c"]);
    let prs = env.prs_for(&changes);
    std::fs::write(env.work.join("b.txt"), "b, edited\n").unwrap();
    env.jj(&["squash", "--into", &changes[1]]);
    std::fs::write(env.work.join("c.txt"), "c, edited\n").unwrap();
    env.jj(&["squash", "--into", &changes[2]]);
    env.spr(&["diff", "--all", "-m", "edit B and C"]);
    let show = |env: &TestEnv, n: u64, path: &str| {
        env.remote_git(&["show", &format!("{}:{path}", env.head_of(n))])
    };
    assert_eq!(show(&env, prs[2], "c.txt"), "c, edited");

    env.spr(&["land", "-r", &changes[0]]);
    assert_eq!(
        show(&env, prs[2], "c.txt"),
        "c, edited",
        "C's edit was in a merge commit that GitHub's rebase dropped"
    );
    assert_eq!(env.pr_files(prs[2]), vec!["c.txt"]);
}

#[test]
fn land_reports_branch_it_could_not_delete() {
    let env = TestEnv::new();
    let changes = env.build_stack(&["a", "b"]);
    let prs = env.prs_for(&changes);
    let branch = env.pr(prs[0]).head_ref;
    let hook = env.root.path().join("remote.git/hooks/pre-receive");
    std::fs::write(
        &hook,
        "#!/bin/sh\nwhile read old new ref; do\n  case $new in 0000000000000000000000000000000000000000)\n    echo \"deletion of $ref denied\" >&2; exit 1;;\n  esac\ndone\n",
    )
    .unwrap();
    std::process::Command::new("chmod")
        .arg("+x")
        .arg(&hook)
        .status()
        .unwrap();

    let out = env.spr(&["land", "-r", &changes[0]]);
    assert!(
        out.flat()
            .contains(&format!("Could not delete branch {branch}")),
        "{}",
        out.text
    );
    assert!(
        out.flat().contains("denied"),
        "git's reason should be shown:\n{}",
        out.text
    );
    assert!(env.branch(&branch).is_some());
}

#[test]
fn land_keeps_branches_when_open_pull_requests_cannot_be_listed() {
    // Without a stack nothing but jj-spr's cleanup moves B off A's branch,
    // and deleting that branch would close B.
    let env = TestEnv::with_stacks(false);
    let changes = env.build_stack(&["a", "b"]);
    let prs = env.prs_for(&changes);
    let branch = env.pr(prs[0]).head_ref;
    env.gh
        .state()
        .failing_queries
        .push("OpenPullRequestBranchesQuery".into());

    let out = env.spr(&["land", "-r", &changes[0]]);
    assert!(env.pr(prs[0]).merged_at.is_some(), "A should be merged");
    let text = out.flat();
    assert!(
        text.contains("Could not list the open Pull Requests"),
        "{}",
        out.text
    );
    assert!(text.contains(&branch), "{}", out.text);
    assert!(env.branch(&branch).is_some(), "A's branch was deleted");
    let dependent = env.pr(prs[1]);
    assert!(
        dependent.is_open(),
        "B was closed: {:?}",
        dependent.closed_reason
    );
}

#[test]
fn land_waits_for_github_to_compute_mergeability() {
    let env = TestEnv::new();
    let changes = env.build_stack(&["a", "b"]);
    let prs = env.prs_for(&changes);
    env.gh.state().unknown_mergeability_polls = 30;

    let out = env.spr(&["land", "-r", &changes[0]]);
    assert!(out.flat().contains("Waiting for GitHub"), "{}", out.text);
    assert!(env.pr(prs[0]).merged_at.is_some(), "A should be merged");
}

#[test]
fn land_gives_up_when_github_does_not_compute_mergeability() {
    let env = TestEnv::new();
    let changes = env.build_stack(&["a", "b"]);
    let prs = env.prs_for(&changes);
    env.gh.state().unknown_mergeability_polls = u32::MAX;

    let out = env.try_spr(&["land", "-r", &changes[0]]);
    assert!(!out.success, "land should give up:\n{}", out.text);
    assert!(
        out.flat()
            .contains("GitHub has not finished computing whether"),
        "{}",
        out.text
    );
    assert!(env.pr(prs[0]).is_open());
}

#[test]
fn land_stops_at_a_pull_request_github_closed() {
    // A, B, C reordered to A, C, B and pushed the way older versions of
    // jj-spr did: C's branch, which has B's commit in it, gets a commit that
    // takes B's file out, and B's branch gets a merge with C's. After A
    // merges, GitHub's rebase finds no commits of B's own and closes B.
    let env = TestEnv::new();
    let changes = env.build_stack(&["a", "b", "c"]);
    let prs = env.prs_for(&changes);
    let (b_head, c_head) = (env.head_of(prs[1]), env.head_of(prs[2]));
    env.jj(&["rebase", "-r", &changes[1], "--insert-after", &changes[2]]);
    env.jj(&["new", &changes[1]]);
    let git = |args: &[&str]| env.git_in(&env.work, args);
    let c_update = git(&[
        "commit-tree",
        &env.tree_of(&changes[2]),
        "-p",
        &c_head,
        "-m",
        "reorder",
    ]);
    let b_update = git(&[
        "commit-tree",
        &env.tree_of(&changes[1]),
        "-p",
        &b_head,
        "-p",
        &c_update,
        "-m",
        "reorder",
    ]);
    let branches: Vec<String> = prs.iter().map(|&n| env.pr(n).head_ref).collect();
    env.push_commit(&c_update, &branches[2]);
    env.push_commit(&b_update, &branches[1]);
    {
        let mut state = env.gh.state();
        state.stacks.clear();
        state.prs.get_mut(&prs[2]).unwrap().base_ref = branches[0].clone();
        state.prs.get_mut(&prs[1]).unwrap().base_ref = branches[2].clone();
        state.stack_outside_spr(&[prs[0], prs[2], prs[1]]);
    }
    let main = env.branch("main").unwrap();

    let out = env.try_spr(&["land", "-r", &changes[1], "--yes"]);
    assert!(!out.success, "land should stop:\n{}", out.text);
    let text = out.flat();
    assert!(
        text.contains(&format!(
            "Landed #{}, #{}, but not #{}",
            prs[0], prs[2], prs[1]
        )),
        "{}",
        out.text
    );
    assert!(
        text.contains(&format!(
            "Pull Request #{} was closed without being merged",
            prs[1]
        )),
        "{}",
        out.text
    );
    assert!(!text.contains("has not finished computing"), "{}", out.text);
    assert_eq!(
        env.landed_since(&main),
        vec!["Change A: a.txt", "Change C: c.txt"]
    );

    // The way out: sync, then a new Pull Request for B. Its old branch is
    // still on GitHub, so the new one gets another name.
    let main = env.branch("main").unwrap();
    let out = env.spr(&["sync"]);
    assert!(
        out.flat().contains(&format!(
            "#{} was closed without merging; keeping it",
            prs[1]
        )),
        "{}",
        out.text
    );
    assert!(!env.is_visible(&changes[0]) && !env.is_visible(&changes[2]));
    let out = env.try_spr(&["diff", "--all", "-m", "reopen B"]);
    assert!(!out.success, "diff should refuse:\n{}", out.text);
    assert!(
        out.flat().contains(&format!("#{} is closed", prs[1])),
        "{}",
        out.text
    );
    env.jj(&["describe", "-r", &changes[1], "-m", "Change B"]);
    env.spr(&["diff", "--all", "-m", "reopen B"]);
    let reopened = env.pr_for(&changes[1]);
    assert_ne!(reopened, prs[1], "B gets a new Pull Request");
    assert_ne!(env.pr(reopened).head_ref, branches[1]);
    env.check_pr(reopened, "main", &["b.txt"], &changes[1]);
    env.spr(&["land", "-r", &changes[1]]);
    assert_eq!(env.landed_since(&main), vec!["Change B: b.txt"]);
}

#[test]
fn land_refuses_when_github_requires_a_review() {
    // Even with spr.requireApproval off, GitHub itself refuses to merge a
    // Pull Request whose review decision is not satisfied; stop early.
    let env = TestEnv::new();
    let changes = env.build_stack(&["a", "b"]);
    let prs = env.prs_for(&changes);
    env.gh.state().review(prs[0], "bob", "CHANGES_REQUESTED");

    let out = env.try_spr(&["land", "-r", &changes[0]]);
    assert!(!out.success, "land should refuse:\n{}", out.text);
    assert!(
        out.flat()
            .contains(&format!("#{} has changes requested", prs[0])),
        "{}",
        out.text
    );
    assert!(env.pr(prs[0]).is_open());
}

#[test]
fn land_needs_an_approval_when_the_repository_requires_one() {
    let env = TestEnv::new();
    env.jj(&["config", "set", "--repo", "spr.requireApproval", "true"]);
    let a = env.commit("Change A", "a.txt", "a\n");
    env.spr(&["diff", "-r", &a, "-m", "A"]);
    let pr = env.pr_for(&a);

    let out = env.try_spr(&["land", "-r", &a]);
    assert!(!out.success, "land should refuse:\n{}", out.text);
    assert!(
        out.flat().contains(&format!("#{pr} has not been approved")),
        "{}",
        out.text
    );
    assert!(env.pr(pr).is_open());

    env.gh.state().review(pr, "bob", "APPROVED");
    env.spr(&["land", "-r", &a]);
    assert!(env.pr(pr).merged_at.is_some(), "A should be merged");
}

#[test]
fn diff_refuses_until_merged_changes_are_synced() {
    // Merged on GitHub, but the local stack still has the merged change.
    let env = TestEnv::new();
    let changes = env.build_stack(&["a", "b", "c"]);
    let prs = env.prs_for(&changes);
    env.gh.state().merge_on_github(prs[0]);
    let before: Vec<String> = prs[1..].iter().map(|&n| env.head_of(n)).collect();
    let stacks_before = env.active_stacks();

    let out = env.try_spr(&["diff", "--all", "-m", "before sync"]);
    assert!(!out.success, "diff should refuse:\n{}", out.text);
    assert!(
        out.flat()
            .contains(&format!("#{} has already merged", prs[0])),
        "{}",
        out.text
    );
    assert!(out.flat().contains("jj spr sync"), "{}", out.text);
    let after: Vec<String> = prs[1..].iter().map(|&n| env.head_of(n)).collect();
    assert_eq!(before, after, "nothing may be pushed");
    assert_eq!(env.active_stacks(), stacks_before, "the stack must be kept");

    env.spr(&["sync"]);
    env.spr(&["diff", "--all", "-m", "after sync"]);
    env.check_chain(&changes[1..]);
}

#[test]
fn land_after_resolving_a_conflict_with_the_change_below() {
    // A changes line 1 of f.txt, B line 3, and C line 5. A is then amended
    // to change line 3 too, and the conflict in B resolved. After A merges,
    // GitHub cannot replay B onto main, so it rebases nothing, and B's Pull
    // Request conflicts with main until sync and diff merge main into it.
    let env = TestEnv::new();
    env.push_to_main("f.txt", "1\n2\n3\n4\n5\n");
    env.jj(&["git", "fetch"]);
    env.jj(&["new", "main@origin"]);
    let changes = vec![
        env.commit("Change A", "f.txt", "1 A\n2\n3\n4\n5\n"),
        env.commit("Change B", "f.txt", "1 A\n2\n3 B\n4\n5\n"),
        env.commit("Change C", "f.txt", "1 A\n2\n3 B\n4\n5 C\n"),
    ];
    env.spr(&["diff", "--all", "-m", "initial"]);
    let prs = env.prs_for(&changes);
    let main = env.branch("main").unwrap();
    env.jj(&["edit", &changes[0]]);
    std::fs::write(env.work.join("f.txt"), "1 A\n2\n3 A\n4\n5\n").unwrap();
    env.jj(&["edit", &changes[1]]);
    std::fs::write(env.work.join("f.txt"), "1 A\n2\n3 A+B\n4\n5\n").unwrap();
    env.jj(&["new", &changes[2]]);
    env.spr(&["diff", "--all", "-m", "amend A, resolve B"]);
    env.check_chain(&changes);
    let heads: Vec<String> = prs[1..].iter().map(|&n| env.head_of(n)).collect();
    let a_branch = env.pr(prs[0]).head_ref;

    let out = env.try_spr(&["land", "-r", &changes[2], "--yes"]);
    assert!(!out.success, "land should stop after A:\n{}", out.text);
    let text = out.flat();
    assert!(
        text.contains(&format!(
            "Landed #{}, but not #{}, #{}",
            prs[0], prs[1], prs[2]
        )),
        "{}",
        out.text
    );
    assert!(
        text.contains(&format!("#{} is not mergeable", prs[1])),
        "{}",
        out.text
    );
    assert!(
        env.requests_since(0, "PUT", &format!("pulls/{}/merge", prs[1]))
            .is_empty(),
        "land should not ask GitHub to merge a conflicting Pull Request"
    );
    assert!(env.pr(prs[0]).merged_at.is_some(), "A should be merged");
    assert!(
        env.branch(&a_branch).is_none(),
        "the landed branch should be deleted"
    );
    let after: Vec<String> = prs[1..].iter().map(|&n| env.head_of(n)).collect();
    assert_eq!(after, heads, "GitHub rebases nothing when B conflicts");
    assert_eq!(env.pr(prs[1]).base_ref, "main");
    assert_eq!(env.pr(prs[2]).base_ref, env.pr(prs[1]).head_ref);

    env.spr(&["sync"]);
    env.spr(&["diff", "--all", "-m", "after sync"]);
    env.check_chain(&changes[1..]);
    env.spr(&["land", "-r", &changes[2], "--yes"]);
    assert_eq!(
        env.landed_since(&main),
        vec!["Change A: f.txt", "Change B: f.txt", "Change C: f.txt"]
    );
    assert_eq!(
        env.remote_git(&["show", "main:f.txt"]),
        "1 A\n2\n3 A+B\n4\n5 C"
    );
}

#[test]
fn amend_the_bottom_change_then_land_it_and_the_rest() {
    // The amend gives B and C a merge commit each. After A merges, GitHub
    // replays B and C without those, which is what the local changes are
    // after a sync, so there is nothing to push before landing the rest.
    let env = TestEnv::new();
    let changes = env.build_stack(&["a", "b", "c"]);
    let prs = env.prs_for(&changes);
    let main = env.branch("main").unwrap();
    std::fs::write(env.work.join("a.txt"), "a, amended\n").unwrap();
    env.jj(&["squash", "--into", &changes[0]]);
    env.spr(&["diff", "--all", "-m", "amend A"]);

    env.spr(&["land", "-r", &changes[0]]);
    env.spr(&["sync"]);
    let heads: Vec<String> = prs[1..].iter().map(|&n| env.head_of(n)).collect();
    let out = env.spr(&["diff", "--all", "-m", "after sync"]);
    assert_eq!(
        out.text.matches("No update necessary").count(),
        2,
        "{}",
        out.text
    );
    let after: Vec<String> = prs[1..].iter().map(|&n| env.head_of(n)).collect();
    assert_eq!(after, heads, "nothing should be pushed");

    env.spr(&["land", "-r", &changes[2], "--yes"]);
    assert_eq!(
        env.landed_since(&main),
        vec!["Change A: a.txt", "Change B: b.txt", "Change C: c.txt"]
    );
    assert_eq!(env.remote_git(&["show", "main:a.txt"]), "a, amended");
}

#[test]
fn insert_a_pull_request_in_the_middle_and_land() {
    let env = TestEnv::new();
    let changes = env.build_stack(&["a", "b", "c"]);
    let main = env.branch("main").unwrap();
    env.jj(&["new", "--insert-after", &changes[0], "-m", "Change X"]);
    std::fs::write(env.work.join("x.txt"), "x\n").unwrap();
    let x = env.change_id("@");
    env.jj(&["new", &changes[2]]);

    env.spr(&["diff", "--all", "-m", "insert X"]);
    env.check_chain(&strings(&[&changes[0], &x, &changes[1], &changes[2]]));
    env.spr(&["land", "-r", &changes[2], "--yes"]);
    assert_eq!(
        env.landed_since(&main),
        vec![
            "Change A: a.txt",
            "Change X: x.txt",
            "Change B: b.txt",
            "Change C: c.txt"
        ]
    );
}

#[test]
fn remove_a_pull_request_from_the_middle_and_land() {
    // B is closed and abandoned. C's branch has B's commit in it, so C's
    // update takes b.txt out again, and C lands without it.
    let env = TestEnv::new();
    let changes = env.build_stack(&["a", "b", "c"]);
    let prs = env.prs_for(&changes);
    let main = env.branch("main").unwrap();
    let b_branch = env.pr(prs[1]).head_ref;
    let c_head = env.head_of(prs[2]);

    env.spr(&["close", "-r", &changes[1]]);
    assert!(!env.pr(prs[1]).is_open(), "B should be closed");
    assert!(
        env.branch(&b_branch).is_some(),
        "C still targets B's branch"
    );
    assert!(env.pr(prs[2]).is_open(), "C should stay open");
    assert_eq!(env.stacks().values().next(), Some(&prs), "B stays listed");

    env.jj(&["abandon", &changes[1]]);
    env.spr(&["diff", "--all", "-m", "remove B"]);
    env.check_chain(&strings(&[&changes[0], &changes[2]]));
    assert!(
        env.is_ancestor(&c_head, &env.head_of(prs[2])),
        "C's update should be a fast-forward"
    );
    env.spr(&["land", "-r", &changes[2], "--yes"]);
    assert_eq!(
        env.landed_since(&main),
        vec!["Change A: a.txt", "Change C: c.txt"]
    );
}

#[test]
fn move_changes_out_of_a_stack() {
    // K and L move off J onto main. GitHub does not allow changing K's base
    // while it is in the stack, and dissolving the stack needs J in the run.
    let env = TestEnv::new();
    let changes = env.build_stack(&["j", "k", "l"]);
    let prs = env.prs_for(&changes);
    let j_head = env.head_of(prs[0]);
    env.jj(&["rebase", "-s", &changes[1], "-d", "main@origin"]);
    env.jj(&["new", &changes[2]]);
    let stacks = env.active_stacks();

    let out = env.try_spr(&["diff", "--all", "-m", "split"]);
    assert!(!out.success, "diff should refuse:\n{}", out.text);
    assert!(
        out.flat().contains(&format!("also contains #{}", prs[0])),
        "{}",
        out.text
    );
    assert_eq!(env.active_stacks(), stacks, "the stack must be kept");

    let selection = format!("{} | {}::{}", changes[0], changes[1], changes[2]);
    env.spr(&["diff", "-r", &selection, "-m", "split"]);
    env.check_chain(&changes[1..]);
    assert_eq!(env.active_stacks(), vec![prs[1..].to_vec()]);
    assert!(env.pr(prs[0]).is_open());
    assert_eq!(env.head_of(prs[0]), j_head, "J should not change");
}

#[test]
fn close_deletes_the_branch_and_unlinks_the_change() {
    let env = TestEnv::new();
    let a = env.commit("Change A", "a.txt", "a\n");
    env.spr(&["diff", "-r", &a, "-m", "A"]);
    let pr = env.pr_for(&a);
    let branch = env.pr(pr).head_ref;

    env.spr(&["close", "-r", &a]);
    assert!(!env.pr(pr).is_open());
    assert!(env.pr(pr).merged_at.is_none());
    assert!(
        env.branch(&branch).is_none(),
        "the branch should be deleted"
    );
    let description = env.jj_query(&["log", "--no-graph", "-r", &a, "-T", "description"]);
    assert_eq!(description.trim(), "Change A");
    assert_eq!(env.files_in(&a), vec!["a.txt"], "the change is kept");
}
