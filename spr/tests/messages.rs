//! End-to-end tests of keeping a change's description and its Pull Request's
//! title and body the same: the warning from `jj spr diff`, `diff
//! --update-message`, and `jj spr amend`, against a fake GitHub (see
//! `common/fake_github.rs`). They need `jj` and `git` on `PATH`.

mod common;

use common::env::TestEnv;

/// A submitted change, "Change A" with a body. Returns it and its Pull
/// Request.
fn submitted(env: &TestEnv) -> (String, u64) {
    let a = env.commit("Change A\n\nWhat A does.", "a.txt", "a\n");
    env.spr(&["diff", "-r", &a]);
    let number = env.pr_for(&a);
    let pr = env.pr(number);
    assert_eq!(pr.title, "Change A");
    assert_eq!(pr.body.trim(), "What A does.");
    (a, number)
}

#[test]
fn diff_warns_about_a_new_description_and_update_message_pushes_it() {
    let env = TestEnv::new();
    let (a, number) = submitted(&env);
    let head = env.head_of(number);
    let text = env
        .description(&a)
        .replace("Change A", "Better title")
        .replace("What A does.", "Better body.");
    env.jj(&["describe", "-r", &a, "-m", &text]);

    let out = env.spr(&["diff", "-r", &a]);
    assert!(out.flat().contains("differ"), "{}", out.text);
    assert!(out.flat().contains("--update-message"), "{}", out.text);
    assert_eq!(env.pr(number).title, "Change A", "GitHub keeps its title");

    env.spr(&["diff", "-r", &a, "--update-message"]);
    let pr = env.pr(number);
    assert_eq!(pr.title, "Better title");
    assert_eq!(pr.body.trim(), "Better body.");
    assert_eq!(env.head_of(number), head, "nothing should be pushed");
    let out = env.spr(&["diff", "-r", &a]);
    assert!(!out.flat().contains("differ"), "{}", out.text);
}

#[test]
fn amend_takes_the_title_and_body_from_github() {
    let env = TestEnv::new();
    let (a, number) = submitted(&env);
    let tree = env.tree_of(&a);
    {
        let mut state = env.gh.state();
        let pr = state.prs.get_mut(&number).unwrap();
        pr.title = "Edited on GitHub".into();
        pr.body = "Edited body.".into();
    }

    env.spr(&["amend", "-r", &a]);
    let description = env.description(&a);
    assert!(
        description.starts_with("Edited on GitHub\n\nEdited body.\n"),
        "{description}"
    );
    assert_eq!(env.pr_for(&a), number, "the link is kept");
    assert_eq!(env.tree_of(&a), tree, "only the description changes");
    let out = env.spr(&["diff", "-r", &a]);
    assert!(!out.flat().contains("differ"), "{}", out.text);
    assert_eq!(env.pr(number).title, "Edited on GitHub");
}
