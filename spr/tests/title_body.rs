//! End-to-end tests of `jj spr diff --title` and `--body`, which set one Pull
//! Request's title and body on GitHub in place of the top change's
//! description, against a fake GitHub (see `common/fake_github.rs`). They
//! need `jj` and `git` on `PATH`.

mod common;

use common::env::TestEnv;

/// A change's description without its `Pull Request:` line.
fn description_without_link(env: &TestEnv, change: &str) -> String {
    env.description(change)
        .lines()
        .filter(|line| !line.starts_with("Pull Request:"))
        .collect::<Vec<_>>()
        .join("\n")
        .trim()
        .to_string()
}

const TITLE: &str = "Extract the HTTP app";
const BODY: &str = "Built on the new router.\n\nSee the design notes.";

#[test]
fn a_new_pull_request_takes_the_given_title_and_body() {
    let env = TestEnv::new();
    let a = env.commit("Change A\n\nWhat A does.", "a.txt", "a\n");

    let out = env.spr(&["diff", "-r", &a, "--title", TITLE, "--body", BODY]);
    let pr = env.pr(env.pr_for(&a));
    assert_eq!(pr.title, TITLE);
    assert_eq!(pr.body, BODY);
    assert_eq!(
        description_without_link(&env, &a),
        "Change A\n\nWhat A does."
    );
    assert!(!out.text.contains("differ"), "{}", out.text);
}

#[test]
fn only_the_title_or_only_the_body_is_replaced() {
    let env = TestEnv::new();
    let a = env.commit("Change A\n\nWhat A does.", "a.txt", "a\n");
    let b = env.commit("Change B\n\nWhat B does.", "b.txt", "b\n");

    env.spr(&["diff", "-r", &a, "--title", TITLE]);
    let pr = env.pr(env.pr_for(&a));
    assert_eq!(pr.title, TITLE);
    assert_eq!(pr.body.trim(), "What A does.");

    env.spr(&["diff", "-r", &b, "--body", BODY]);
    let pr = env.pr(env.pr_for(&b));
    assert_eq!(pr.title, "Change B");
    assert_eq!(pr.body, BODY);
}

#[test]
fn an_empty_body_is_allowed() {
    let env = TestEnv::new();
    let a = env.commit("Change A\n\nWhat A does.", "a.txt", "a\n");
    env.spr(&["diff", "-r", &a, "--body", ""]);
    let pr = env.pr(env.pr_for(&a));
    assert_eq!(pr.title, "Change A");
    assert_eq!(pr.body, "");
}

#[test]
fn one_pr_takes_the_given_title_and_body_and_keeps_each_step() {
    let env = TestEnv::new();
    let b: Vec<String> = ["b1", "b2", "b3"]
        .iter()
        .map(|name| env.commit(&format!("Change {name}"), &format!("{name}.txt"), "b\n"))
        .collect();

    let range = format!("{}::{}", b[0], b[2]);
    env.spr(&[
        "diff", "-r", &range, "--one-pr", "--title", TITLE, "--body", BODY,
    ]);
    let number = env.pr_for(&b[2]);
    let pr = env.pr(number);
    assert_eq!(pr.title, TITLE);
    assert_eq!(pr.body, BODY);
    assert_eq!(
        env.pr_commits(number),
        vec!["Change b1", "Change b2", "Change b3"]
    );
    for (change, name) in b.iter().zip(["b1", "b2", "b3"]) {
        assert_eq!(env.pr_for(change), number);
        assert_eq!(
            description_without_link(&env, change),
            format!("Change {name}")
        );
    }
}

#[test]
fn an_existing_pull_request_gets_the_given_title_and_body() {
    let env = TestEnv::new();
    let a = env.commit("Change A\n\nWhat A does.", "a.txt", "a\n");
    env.spr(&["diff", "-r", &a]);
    let number = env.pr_for(&a);
    let head = env.head_of(number);

    // No content change: only the title and body are updated.
    let out = env.spr(&["diff", "-r", &a, "--title", TITLE, "--body", BODY]);
    assert!(out.text.contains("No update necessary"), "{}", out.text);
    assert_eq!(env.head_of(number), head, "nothing should be pushed");
    let pr = env.pr(number);
    assert_eq!((pr.title.as_str(), pr.body.as_str()), (TITLE, BODY));

    // A content change, while GitHub's title already differs from the
    // description: no warning about that in this run.
    std::fs::write(env.work.join("a.txt"), "a, edited\n").unwrap();
    env.jj(&["squash", "--into", &a]);
    let out = env.spr(&[
        "diff",
        "-r",
        &a,
        "--title",
        "Second title",
        "--body",
        "Second body",
        "-m",
        "edit",
    ]);
    assert!(!out.text.contains("differ"), "{}", out.text);
    assert_ne!(env.head_of(number), head, "the edit should be pushed");
    let pr = env.pr(number);
    assert_eq!(pr.title, "Second title");
    assert_eq!(pr.body, "Second body");
    assert_eq!(
        description_without_link(&env, &a),
        "Change A\n\nWhat A does."
    );
}

#[test]
fn a_run_with_several_pull_requests_is_refused() {
    let env = TestEnv::new();
    let a = env.commit("Change A", "a.txt", "a\n");
    let b = env.commit("Change B", "b.txt", "b\n");

    let out = env.try_spr(&["diff", "--all", "--title", TITLE]);
    assert!(!out.success, "diff should refuse:\n{}", out.text);
    assert!(out.flat().contains("once per Pull Request"), "{}", out.text);
    assert!(
        env.gh.state().prs.is_empty(),
        "no Pull Request may be opened"
    );
    assert_eq!(
        env.gh.state().branches().keys().collect::<Vec<_>>(),
        vec!["main"],
        "nothing may be pushed"
    );
    assert_eq!(env.description(&a).trim(), "Change A");
    assert_eq!(env.description(&b).trim(), "Change B");

    let tips = format!("{a} | {b}");
    let out = env.try_spr(&["diff", "--tips", &tips, "--body", BODY]);
    assert!(!out.success, "diff should refuse:\n{}", out.text);
    assert!(
        env.gh.state().prs.is_empty(),
        "no Pull Request may be opened"
    );
}

#[test]
fn tips_with_one_tip_take_the_given_title() {
    let env = TestEnv::new();
    env.commit("Change A", "a.txt", "a\n");
    let b = env.commit("Change B", "b.txt", "b\n");
    env.spr(&["diff", "--tips", &b, "--title", TITLE]);
    assert_eq!(env.pr(env.pr_for(&b)).title, TITLE);
}

#[test]
fn land_uses_the_given_title_and_body() {
    let env = TestEnv::new();
    let a = env.commit("Change A\n\nWhat A does.", "a.txt", "a\n");
    env.spr(&["diff", "-r", &a, "--title", TITLE, "--body", BODY]);
    let number = env.pr_for(&a);
    let main = env.branch("main").unwrap();

    env.spr(&["land", "-r", &a]);
    let message = env.remote_git(&["log", "-1", "--format=%B", "main"]);
    assert_eq!(env.remote_git(&["rev-parse", "main~1"]), main);
    assert_eq!(
        message.trim(),
        format!(
            "{TITLE}\n\n{BODY}\n\nReviewers: \n\nPull Request: \
             https://github.com/fake/repo/pull/{number}"
        )
    );
}

#[test]
fn dry_run_shows_the_given_title() {
    let env = TestEnv::new();
    let a = env.commit("Change A", "a.txt", "a\n");
    let out = env.spr(&["diff", "-r", &a, "--title", TITLE, "--dry-run"]);
    assert!(out.text.contains(&format!("\"{TITLE}\"")), "{}", out.text);
    // A new Pull Request's branch is named after its title.
    assert!(
        out.text.contains("head: spr/extract-the-http-app"),
        "{}",
        out.text
    );
    assert!(env.gh.state().prs.is_empty());
    assert_eq!(env.description(&a).trim(), "Change A");
}
