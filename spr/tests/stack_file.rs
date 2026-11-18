//! End-to-end tests of stack files (`jj spr stack-file`, `jj spr submit`,
//! and `land`, `sync`, and `close` with a stack file) against a fake GitHub
//! (see `common/fake_github.rs`). They need `jj` and `git` on `PATH`. Set
//! `SPR_TEST_VERBOSE=1` to print every command.

mod common;

use std::path::PathBuf;

use common::env::{Outcome, TestEnv};
use serde_json::{Value, json};

/// A stack file and its body files, in a directory outside the repository.
struct Stack<'a> {
    env: &'a TestEnv,
    dir: PathBuf,
    path: String,
}

impl<'a> Stack<'a> {
    fn new(env: &'a TestEnv) -> Self {
        let dir = env.root.path().join("stack");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("stack.json").display().to_string();
        Self { env, dir, path }
    }

    fn write(&self, file: Value) {
        std::fs::write(&self.path, serde_json::to_string_pretty(&file).unwrap()).unwrap();
    }

    /// Write the stack file with these entries.
    fn entries(&self, entries: Value) {
        self.write(json!({ "prs": entries }));
    }

    fn body(&self, name: &str, text: &str) {
        std::fs::write(self.dir.join(name), text).unwrap();
    }

    fn read(&self) -> Value {
        serde_json::from_str(&std::fs::read_to_string(&self.path).unwrap()).unwrap()
    }

    fn entry(&self, index: usize) -> Value {
        self.read()["prs"][index].clone()
    }

    /// The `pr` of each entry.
    fn prs(&self) -> Vec<u64> {
        self.read()["prs"]
            .as_array()
            .unwrap()
            .iter()
            .map(|e| e["pr"].as_u64().expect("an entry without a pr"))
            .collect()
    }

    /// Change one entry's keys.
    fn edit(&self, index: usize, keys: Value) {
        let mut file = self.read();
        for (key, value) in keys.as_object().unwrap() {
            file["prs"][index][key] = value.clone();
        }
        self.write(file);
    }

    fn spr(&self, command: &str, args: &[&str]) -> Outcome {
        let mut full = vec![command, self.path.as_str()];
        full.extend(args);
        self.env.spr(&full)
    }

    fn try_spr(&self, command: &str, args: &[&str]) -> Outcome {
        let mut full = vec![command, self.path.as_str()];
        full.extend(args);
        self.env.try_spr(&full)
    }

    fn status(&self) -> Outcome {
        self.env
            .try_spr(&["stack-file", "status", self.path.as_str()])
    }

    fn status_json(&self) -> Value {
        let output = std::process::Command::new(env!("CARGO_BIN_EXE_jj-spr"))
            .args(["stack-file", "status", self.path.as_str(), "--json"])
            .current_dir(&self.env.work)
            .envs(self.env.env_vars())
            .output()
            .unwrap();
        serde_json::from_slice(&output.stdout).unwrap_or_else(|e| {
            panic!(
                "status --json printed no JSON ({e}):\n{}{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            )
        })
    }
}

fn heads(env: &TestEnv, prs: &[u64]) -> Vec<String> {
    prs.iter().map(|&n| env.head_of(n)).collect()
}

/// Nothing was pushed or opened since `before` (the heads of `prs`).
fn nothing_pushed(env: &TestEnv, prs: &[u64], before: &[String]) {
    assert_eq!(heads(env, prs), before, "nothing may be pushed");
    assert_eq!(env.gh.state().prs.len(), prs.len(), "nothing may be opened");
}

/// A change titled "Change <name>" that adds `<name>.txt`.
fn change(env: &TestEnv, name: &str) -> String {
    env.commit(
        &format!("Change {name}"),
        &format!("{name}.txt"),
        &format!("{name}\n"),
    )
}

// -- init and status ----------------------------------------------------------

#[test]
fn init_writes_one_entry_per_tip() {
    let env = TestEnv::new();
    let a = change(&env, "a");
    let b1 = change(&env, "b1");
    let b2 = change(&env, "b2");
    let c = change(&env, "c");
    env.jj(&["bookmark", "create", "-r", &b2, "feature-b"]);
    let stack = Stack::new(&env);

    let tips = format!("{a} | {b2} | {c}");
    env.spr(&["stack-file", "init", &stack.path, "--tips", &tips]);
    assert_eq!(
        stack.read(),
        json!({"prs": [
            {"tip": a[..12], "title": "Change a"},
            {"tip": "feature-b", "title": "Change b2"},
            {"tip": c[..12], "title": "Change c"},
        ]})
    );
    let out = env.try_spr(&["stack-file", "init", &stack.path, "--tips", &tips]);
    assert!(!out.success, "init should refuse:\n{}", out.text);
    assert!(out.text.contains("already exists"), "{}", out.text);

    let status = stack.status();
    assert!(status.success, "{}", status.text);
    assert!(status.text.contains("Change b1"), "{}", status.text);
    assert!(status.text.contains(&b1[..8]), "{}", status.text);
    assert_eq!(
        status.text.matches("open a Pull Request").count(),
        3,
        "{}",
        status.text
    );
}

#[test]
fn init_needs_tips_in_a_line() {
    let env = TestEnv::new();
    let a = change(&env, "a");
    env.jj(&["new", "trunk()"]);
    let x = change(&env, "x");
    let stack = Stack::new(&env);
    let out = env.try_spr(&[
        "stack-file",
        "init",
        &stack.path,
        "--tips",
        &format!("{a} | {x}"),
    ]);
    assert!(!out.success, "init should refuse:\n{}", out.text);
    assert!(out.flat().contains("in a line"), "{}", out.text);
    assert!(!std::path::Path::new(&stack.path).exists());
}

#[test]
fn status_lists_every_problem() {
    let env = TestEnv::new();
    let a = change(&env, "a");
    let undescribed = env.commit("", "b.txt", "b\n");
    let c = env.commit(
        "Change c\n\nPull Request: https://github.com/fake/repo/pull/9",
        "c.txt",
        "c\n",
    );
    let stack = Stack::new(&env);
    // An entry's changes are unknown when the tip below it doesn't
    // resolve, so the entries with bad tips go on top.
    stack.entries(json!([
        {"tip": a, "title": "A"},
        {"tip": undescribed, "body": "b.md"},
        {"tip": c, "title": "C"},
        {"tip": "@-"},
        {"tip": "nothing-by-this-name", "title": "Missing"},
    ]));
    stack.body("b.md", "A body without a heading.\n");

    let out = stack.status();
    assert!(!out.success, "status should fail:\n{}", out.text);
    let text = out.flat();
    for expected in [
        "relative to the working copy",
        "not a local bookmark",
        "has no title",
        "no description",
        "`Pull Request:` line",
    ] {
        assert!(text.contains(expected), "{expected}: {}", out.text);
    }

    let json = stack.status_json();
    let problems = json["problems"].as_array().unwrap();
    assert_eq!(problems.len(), 6, "{json:#}");
    assert_eq!(json["entries"][0]["title"], "A");
    assert_eq!(json["entries"][0]["title_from"], "the title key");
    assert_eq!(json["entries"][0]["members"][0]["change_id"], a[..8]);
    assert_eq!(json["entries"][0]["actions"][0], "open a Pull Request");
    assert!(env.gh.state().prs.is_empty());
}

#[test]
fn status_refuses_tips_out_of_line_and_duplicates() {
    let env = TestEnv::new();
    let a = change(&env, "a");
    let b = change(&env, "b");
    env.jj(&["new", "trunk()"]);
    let x = change(&env, "x");
    let stack = Stack::new(&env);
    stack.entries(json!([{"tip": b, "title": "B"}, {"tip": a, "title": "A"}]));
    let out = stack.status().flat();
    assert!(out.contains("not in a line"), "{out}");
    assert!(out.contains("has 1 problem."), "{out}");
    stack.entries(json!([{"tip": a, "title": "A"}, {"tip": a, "title": "A again"}]));
    assert!(stack.status().flat().contains("point to the same change"));
    stack.entries(json!([{"tip": a, "title": "A"}, {"tip": x, "title": "X"}]));
    assert!(stack.status().flat().contains("not in a line"));
    stack.entries(json!([{"tip": "trunk", "title": "T"}]));
    env.jj(&["bookmark", "create", "-r", "trunk()", "trunk"]);
    assert!(stack.status().flat().contains("has no changes"));
}

#[test]
fn status_refuses_divergent_and_hidden_tips() {
    let env = TestEnv::new();
    let a = change(&env, "a");
    let b = change(&env, "b");
    env.jj(&["abandon", &b]);
    let stack = Stack::new(&env);
    stack.entries(json!([{"tip": b, "title": "B"}]));
    assert!(stack.status().flat().contains("missing or hidden"));

    let op = env.jj_query(&["op", "log", "--no-graph", "-T", "id ++ \"\\n\""]);
    let op = op.lines().next().unwrap();
    env.jj(&["describe", "-r", &a, "-m", "Change a, once"]);
    env.jj(&["--at-op", op, "describe", "-r", &a, "-m", "Change a, twice"]);
    stack.entries(json!([{"tip": a, "title": "A"}]));
    assert!(stack.status().flat().contains("is divergent"));
}

// -- submit -------------------------------------------------------------------

/// Changes a, b1, b2, b3, and c, with a bookmark `b` on b3, and a stack file
/// of three entries: a and b with body files, c with a title. Returns the
/// changes, grouped by entry.
fn three_entries(env: &TestEnv) -> (Stack<'_>, Vec<Vec<String>>) {
    let a = change(env, "a");
    let b: Vec<String> = ["b1", "b2", "b3"].iter().map(|n| change(env, n)).collect();
    let c = change(env, "c");
    env.jj(&["bookmark", "create", "-r", &b[2], "b"]);
    let stack = Stack::new(env);
    stack.body("a.md", "# Title of A\n\nWhat A does.\n");
    stack.body(
        "b.md",
        "# Title of B\n\nWhat B does.\n\nIn two paragraphs.\n",
    );
    stack.entries(json!([
        {"tip": a, "body": "a.md"},
        {"tip": "b", "body": "b.md", "x-note": "kept"},
        {"tip": c, "title": "Title of C"},
    ]));
    (stack, vec![vec![a], b, vec![c]])
}

/// `three_entries`, submitted. Returns the Pull Requests too.
fn submitted(env: &TestEnv) -> (Stack<'_>, Vec<Vec<String>>, Vec<u64>) {
    let (stack, groups) = three_entries(env);
    stack.spr("submit", &["-m", "unused"]);
    let prs = stack.prs();
    (stack, groups, prs)
}

#[test]
fn submit_opens_a_stack_of_pull_requests() {
    let env = TestEnv::new();
    let (stack, groups) = three_entries(&env);
    let out = stack.spr("submit", &["-m", "unused"]);
    assert_eq!(out.text.matches("Created new").count(), 3, "{}", out.text);

    let prs = stack.prs();
    let titles = ["Title of A", "Title of B", "Title of C"];
    let bodies = ["What A does.", "What B does.\n\nIn two paragraphs.", ""];
    let mut base = "main".to_string();
    for (i, &number) in prs.iter().enumerate() {
        let pr = env.pr(number);
        assert_eq!(
            (pr.title.as_str(), pr.body.as_str()),
            (titles[i], bodies[i])
        );
        let entry = stack.entry(i);
        assert!(entry["synced"].as_str().unwrap().starts_with("sha256:"));
        assert_eq!(entry["pushed"], env.commit_id(groups[i].last().unwrap()));
        assert_eq!(pr.base_ref, base);
        base = pr.head_ref;
    }
    env.check_pr(prs[0], "main", &["a.txt"], &groups[0][0]);
    env.check_pr(
        prs[1],
        &env.pr(prs[0]).head_ref,
        &["b1.txt", "b2.txt", "b3.txt"],
        &groups[1][2],
    );
    assert_eq!(
        env.pr_commits(prs[1]),
        vec!["Change b1", "Change b2", "Change b3"],
        "one commit per change"
    );
    env.check_pr(prs[2], &env.pr(prs[1]).head_ref, &["c.txt"], &groups[2][0]);
    assert_eq!(env.active_stacks(), vec![prs.clone()]);
    for change in groups.iter().flatten() {
        assert!(
            !env.description(change).contains("Pull Request:"),
            "{change}: {}",
            env.description(change)
        );
    }
    assert_eq!(stack.entry(1)["x-note"], "kept");

    // Again: nothing to do.
    let before = heads(&env, &prs);
    let file = stack.read();
    let out = stack.spr("submit", &["-m", "unused"]);
    assert_eq!(
        out.text.matches("No update necessary").count(),
        3,
        "{}",
        out.text
    );
    nothing_pushed(&env, &prs, &before);
    assert_eq!(stack.read(), file);
}

#[test]
fn submit_dry_run_changes_nothing() {
    let env = TestEnv::new();
    let (stack, _) = three_entries(&env);
    let file = stack.read();
    let out = stack.spr("submit", &["--dry-run"]);
    assert_eq!(out.text.matches("CREATE").count(), 3, "{}", out.text);
    assert!(out.text.contains("\"Title of B\""), "{}", out.text);
    assert!(env.gh.state().prs.is_empty());
    assert!(
        !env.gh
            .state()
            .branches()
            .keys()
            .any(|b| b.starts_with("spr/"))
    );
    assert_eq!(stack.read(), file);
}

#[test]
fn submit_appends_to_existing_pull_requests() {
    let env = TestEnv::new();
    let (stack, groups, prs) = submitted(&env);
    let before = heads(&env, &prs);

    // An edit to a lower change: one commit on its Pull Request, and a merge
    // of it in each one above.
    env.jj(&["edit", &groups[0][0]]);
    std::fs::write(env.work.join("a.txt"), "a, revised\n").unwrap();
    env.jj(&["new", &groups[2][0]]);
    stack.spr("submit", &["-m", "revise a"]);
    assert_eq!(env.pr_commits(prs[0]).last().unwrap(), "revise a");
    assert_eq!(
        env.remote_git(&[
            "rev-list",
            "--count",
            &format!("{}..{}", before[0], env.head_of(prs[0]))
        ]),
        "1"
    );
    for i in 1..3 {
        let head = env.head_of(prs[i]);
        let parents = env.remote_git(&["log", "-1", "--format=%P", &head]);
        assert_eq!(
            parents,
            format!("{} {}", before[i], env.head_of(prs[i - 1])),
            "#{} merges the Pull Request below",
            prs[i]
        );
    }
    env.check_pr(prs[0], "main", &["a.txt"], &groups[0][0]);
    env.check_pr(prs[2], &env.pr(prs[1]).head_ref, &["c.txt"], &groups[2][0]);

    // A new change at the top of an entry: a commit of its own.
    env.jj(&["new", "--insert-after", &groups[0][0], "-m", "Change a2"]);
    std::fs::write(env.work.join("a2.txt"), "a2\n").unwrap();
    let a2 = env.change_id("@");
    env.jj(&["new", &groups[2][0]]);
    stack.edit(0, json!({"tip": a2}));
    stack.spr("submit", &["-m", "unused"]);
    assert_eq!(
        env.pr_commits(prs[0]),
        vec!["Change a", "revise a", "Change a2"]
    );
    env.check_pr(prs[0], "main", &["a.txt", "a2.txt"], &a2);
    env.check_pr(
        prs[1],
        &env.pr(prs[0]).head_ref,
        &["b1.txt", "b2.txt", "b3.txt"],
        &groups[1][2],
    );
    assert_eq!(env.active_stacks(), vec![prs.clone()]);
}

/// The `since_pushed` of each change of entry `index` in `status --json`.
fn since_pushed(stack: &Stack, index: usize) -> Vec<String> {
    stack.status_json()["entries"][index]["members"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["since_pushed"].as_str().unwrap_or("(none)").to_string())
        .collect()
}

#[test]
fn status_shows_what_changed_since_the_last_submit() {
    let env = TestEnv::new();
    let (stack, groups, prs) = submitted(&env);
    for i in 0..3 {
        assert!(since_pushed(&stack, i).iter().all(|s| s == "unchanged"));
    }
    assert!(
        stack
            .status()
            .text
            .contains("No local changes since the last submit"),
        "{}",
        stack.status().text
    );

    // In B: b2 edited, b3 reworded, and a new change b4 on top. C is only
    // rebased.
    env.jj(&["edit", &groups[1][1]]);
    std::fs::write(env.work.join("b2.txt"), "b2, revised\n").unwrap();
    env.jj(&["describe", "-r", &groups[1][2], "-m", "Change b3, reworded"]);
    env.jj(&["new", "--insert-after", &groups[1][2], "-m", "Change b4"]);
    std::fs::write(env.work.join("b4.txt"), "b4\n").unwrap();
    env.jj(&["bookmark", "set", "b", "-r", "@"]);
    env.jj(&["new", &groups[2][0]]);
    assert_eq!(since_pushed(&stack, 0), vec!["unchanged"]);
    assert_eq!(
        since_pushed(&stack, 1),
        vec!["unchanged", "edited", "reworded", "new"]
    );
    assert_eq!(since_pushed(&stack, 2), vec!["unchanged"]);
    let out = stack.status();
    assert!(out.success, "{}", out.text);
    for expected in [
        "Change b2 (edited since the last submit)",
        "Change b3, reworded (reworded since the last submit)",
        "Change b4 (new since the last submit)",
    ] {
        assert!(out.text.contains(expected), "{expected}:\n{}", out.text);
    }

    // A commit that is not in the repository.
    stack.edit(
        0,
        json!({"pushed": "0123456789abcdef0123456789abcdef01234567"}),
    );
    assert_eq!(since_pushed(&stack, 0), vec!["unknown"]);
    let out = stack.status();
    assert!(out.success, "{}", out.text);
    assert!(
        out.flat().contains("not in this repository"),
        "{}",
        out.text
    );

    // Submit records the new tips. (Without b4: `submit` takes an edit
    // below a new top change for someone else's push.)
    env.jj(&[
        "bookmark",
        "set",
        "b",
        "--allow-backwards",
        "-r",
        &groups[1][2],
    ]);
    env.jj(&[
        "abandon",
        &format!("{}+ & ::{}", groups[1][2], groups[2][0]),
    ]);
    stack.spr("submit", &["-m", "revise b"]);
    assert_eq!(
        stack.entry(1)["pushed"],
        env.commit_id("b"),
        "{}",
        stack.entry(1)
    );
    assert_eq!(stack.entry(0)["pushed"], env.commit_id(&groups[0][0]));
    for i in 0..3 {
        assert!(since_pushed(&stack, i).iter().all(|s| s == "unchanged"));
    }
    assert_eq!(stack.prs(), prs);
}

#[test]
fn submit_updates_titles_and_bodies() {
    let env = TestEnv::new();
    let (stack, _, prs) = submitted(&env);
    let before = heads(&env, &prs);
    let synced = stack.entry(1)["synced"].clone();

    stack.body("b.md", "# New title of B\n\nNew body of B.\n");
    let out = stack.spr("submit", &["-m", "unused"]);
    assert!(
        out.flat().contains(&format!(
            "Updated the title and body of #{} on GitHub",
            prs[1]
        )),
        "{}",
        out.text
    );
    let pr = env.pr(prs[1]);
    assert_eq!(
        (pr.title.as_str(), pr.body.as_str()),
        ("New title of B", "New body of B.")
    );
    assert_ne!(stack.entry(1)["synced"], synced);
    assert_eq!(heads(&env, &prs), before, "only the title and body change");

    // Edited on GitHub only: GitHub's edit stays, and status and submit
    // say so.
    env.gh.state().prs.get_mut(&prs[1]).unwrap().title = "Edited on GitHub".into();
    let kept = "keep the title edited on GitHub";
    assert!(stack.status().flat().contains(kept));
    assert_eq!(stack.status_json()["entries"][1]["actions"], json!([kept]));
    let out = stack.spr("submit", &["--dry-run"]);
    assert!(
        out.flat().contains(&format!(
            "Would keep the title of #{} as edited on GitHub",
            prs[1]
        )),
        "{}",
        out.text
    );
    let out = stack.spr("submit", &["-m", "unused"]);
    assert!(
        out.flat().contains("Keeping the title of #"),
        "{}",
        out.text
    );
    assert_eq!(env.pr(prs[1]).title, "Edited on GitHub");

    // Edited in both: refused, until --overwrite-github.
    stack.body("b.md", "# Third title of B\n\nNew body of B.\n");
    let out = stack.try_spr("submit", &["-m", "unused"]);
    assert!(!out.success, "submit should refuse:\n{}", out.text);
    assert!(out.flat().contains("--overwrite-github"), "{}", out.text);
    assert_eq!(env.pr(prs[1]).title, "Edited on GitHub");
    stack.spr("submit", &["-m", "unused", "--overwrite-github"]);
    assert_eq!(env.pr(prs[1]).title, "Third title of B");
    stack.spr("submit", &["-m", "unused"]);
}

#[test]
fn submit_upto_pushes_only_the_bottom_entries() {
    let env = TestEnv::new();
    let a = change(&env, "a");
    let b = change(&env, "b");
    let unfinished = env.commit("", "c.txt", "c\n");
    let stack = Stack::new(&env);
    stack.entries(json!([
        {"tip": a, "title": "A"},
        {"tip": b, "title": "B"},
        {"tip": unfinished},
    ]));
    let out = stack.try_spr("submit", &["-m", "unused"]);
    assert!(!out.success, "submit should refuse:\n{}", out.text);
    assert!(env.gh.state().prs.is_empty());

    stack.spr("submit", &["--upto", &b, "-m", "unused"]);
    let file = stack.read();
    let prs: Vec<u64> = (0..2)
        .map(|i| file["prs"][i]["pr"].as_u64().unwrap())
        .collect();
    env.check_pr(prs[0], "main", &["a.txt"], &a);
    env.check_pr(prs[1], &env.pr(prs[0]).head_ref, &["b.txt"], &b);
    assert!(file["prs"][2].get("pr").is_none());
    assert_eq!(env.gh.state().prs.len(), 2);
}

/// The base branch of each of `prs`.
fn bases(env: &TestEnv, prs: &[u64]) -> Vec<String> {
    prs.iter().map(|&n| env.pr(n).base_ref).collect()
}

/// Edit change a of `three_entries`, then `submit --upto <upto>`: the entries
/// up to `upto` (`count` of them) are updated, and the ones above it, their
/// Pull Requests, and the GitHub stack are left as they were. A full submit
/// afterwards gives each entry above one merge of the one below.
fn submit_upto_after_an_edit_below(upto: impl Fn(&[Vec<String>]) -> String, count: usize) {
    let env = TestEnv::new();
    let (stack, groups, prs) = submitted(&env);
    let before = heads(&env, &prs);
    let (bases_before, stacks_before) = (bases(&env, &prs), env.stacks());
    let file_before = stack.read();

    env.jj(&["edit", &groups[0][0]]);
    std::fs::write(env.work.join("a.txt"), "a, revised\n").unwrap();
    env.jj(&["new", &groups[2][0]]);
    let out = stack.spr("submit", &["--upto", &upto(&groups), "-m", "revise a"]);
    assert_eq!(env.pr_commits(prs[0]).last().unwrap(), "revise a");
    for i in 1..count {
        let parents = env.remote_git(&["log", "-1", "--format=%P", &env.head_of(prs[i])]);
        assert_eq!(
            parents,
            format!("{} {}", before[i], env.head_of(prs[i - 1]))
        );
    }
    assert_eq!(heads(&env, &prs)[count..], before[count..], "{}", out.text);
    assert_eq!(bases(&env, &prs), bases_before);
    assert_eq!(env.stacks(), stacks_before, "{}", out.text);
    for i in count..3 {
        assert_eq!(stack.entry(i), file_before["prs"][i]);
    }

    let updated = heads(&env, &prs);
    stack.spr("submit", &["-m", "revise a"]);
    assert_eq!(heads(&env, &prs)[..count], updated[..count]);
    for i in count..3 {
        let parents = env.remote_git(&["log", "-1", "--format=%P", &env.head_of(prs[i])]);
        assert_eq!(
            parents,
            format!("{} {}", before[i], env.head_of(prs[i - 1])),
            "#{} gets one merge of the Pull Request below",
            prs[i]
        );
    }
    assert_eq!(bases(&env, &prs), bases_before);
    assert_eq!(env.stacks(), stacks_before);
    env.check_pr(prs[2], &env.pr(prs[1]).head_ref, &["c.txt"], &groups[2][0]);
}

#[test]
fn submit_upto_the_bottom_leaves_the_entries_above_alone() {
    submit_upto_after_an_edit_below(|groups| groups[0][0].clone(), 1);
}

#[test]
fn submit_upto_the_middle_leaves_the_top_entry_alone() {
    submit_upto_after_an_edit_below(|_| "b".to_string(), 2);
}

#[test]
fn a_rerun_after_a_failed_creation_opens_it_on_a_new_branch() {
    let env = TestEnv::new();
    let a = change(&env, "a");
    let b = change(&env, "b");
    let stack = Stack::new(&env);
    stack.entries(json!([{"tip": a, "title": "A"}, {"tip": b, "title": "B"}]));
    env.gh.state().failing_creations.push("spr/b".into());

    let out = stack.try_spr("submit", &["-m", "unused"]);
    assert!(!out.success, "submit should fail:\n{}", out.text);
    assert!(stack.entry(0)["pr"].is_u64(), "{}", stack.entry(0));
    assert!(stack.entry(1).get("pr").is_none(), "{}", stack.entry(1));
    let orphan = env.branch("spr/b").expect("the branch was pushed");

    stack.spr("submit", &["-m", "unused"]);
    let prs = stack.prs();
    env.check_pr(prs[0], "main", &["a.txt"], &a);
    env.check_pr(prs[1], "spr/a", &["b.txt"], &b);
    assert_eq!(env.pr(prs[1]).head_ref, "spr/b-1");
    assert_eq!(env.active_stacks(), vec![prs]);

    // `cleanup` removes the branch left over from the failed creation.
    assert_eq!(env.branch("spr/b"), Some(orphan));
    env.spr(&["cleanup", "--confirm"]);
    assert!(env.branch("spr/b").is_none(), "cleanup should delete spr/b");
}

/// A branch `name` on GitHub that this repository has never fetched, at a
/// commit of someone else's. Returns the commit.
fn foreign_branch(env: &TestEnv, name: &str) -> String {
    let commit = env.remote_git(&["commit-tree", "main^{tree}", "-m", "Someone else's work"]);
    env.remote_git(&["update-ref", &format!("refs/heads/{name}"), &commit]);
    commit
}

#[test]
fn a_new_pull_request_avoids_a_branch_name_taken_on_github() {
    let env = TestEnv::new();
    let a = change(&env, "a");
    let b = change(&env, "b");
    let foreign = foreign_branch(&env, "spr/a");
    let stack = Stack::new(&env);
    // Two entries with the same title get two names.
    stack.entries(json!([{"tip": a, "title": "A"}, {"tip": b, "title": "A"}]));
    stack.spr("submit", &[]);
    let prs = stack.prs();
    assert_eq!(env.pr(prs[0]).head_ref, "spr/a-1");
    assert_eq!(env.pr(prs[1]).head_ref, "spr/a-2");
    env.check_pr(prs[1], "spr/a-1", &["b.txt"], &b);
    assert_eq!(env.branch("spr/a"), Some(foreign));
}

#[test]
fn a_long_title_gives_a_short_branch_name() {
    let env = TestEnv::new();
    let a = change(&env, "a");
    let title = "A title that goes on and on ".repeat(12);
    let stack = Stack::new(&env);
    stack.entries(json!([{"tip": a, "title": title}]));
    stack.spr("submit", &[]);
    let branch = env.pr(stack.prs()[0]).head_ref;
    assert!(branch.len() <= "spr/".len() + 100, "{branch}");
    let whole = format!("spr/{}", "a-title-that-goes-on-and-on-".repeat(12));
    assert!(
        whole.starts_with(&format!("{branch}-")),
        "cut after a word: {branch}"
    );
}

#[test]
fn submit_refuses_problems_in_the_changes() {
    let env = TestEnv::new();
    let a = env.commit("Change a", "README", "edited locally\n");
    let b = env.commit(
        "Change b\n\nPull Request: https://github.com/fake/repo/pull/9",
        "b.txt",
        "b\n",
    );
    let stack = Stack::new(&env);
    stack.entries(json!([{"tip": a, "title": "A"}, {"tip": b, "title": "B"}, {"tip": "@"}]));
    let out = stack.try_spr("submit", &["-m", "unused"]);
    assert!(!out.success, "submit should refuse:\n{}", out.text);
    let text = out.flat();
    assert!(text.contains("3 problems"), "{}", out.text);
    assert!(text.contains("`Pull Request:` line"), "{}", out.text);
    assert!(
        text.contains("relative to the working copy"),
        "{}",
        out.text
    );
    nothing_pushed(&env, &[], &[]);

    // A conflict, after a rebase onto a main that changed README too.
    stack.entries(json!([{"tip": a, "title": "A"}]));
    env.push_to_main("README", "edited on main\n");
    env.jj(&["git", "fetch"]);
    env.jj(&["rebase", "-s", &a, "-d", "main@origin"]);
    let out = stack.try_spr("submit", &["-m", "unused"]);
    assert!(!out.success, "submit should refuse:\n{}", out.text);
    assert!(out.flat().contains("have conflicts"), "{}", out.text);
    nothing_pushed(&env, &[], &[]);
}

#[test]
fn submit_refuses_a_merged_pull_request() {
    let env = TestEnv::new();
    let (stack, _, prs) = submitted(&env);
    env.gh.state().merge_on_github(prs[0]);
    let before = heads(&env, &prs[1..]);
    let out = stack.try_spr("submit", &["-m", "unused"]);
    assert!(!out.success, "submit should refuse:\n{}", out.text);
    assert!(out.flat().contains("jj spr sync"), "{}", out.text);
    assert_eq!(heads(&env, &prs[1..]), before, "nothing may be pushed");
}

#[test]
fn status_shows_no_action_that_a_problem_makes_meaningless() {
    let env = TestEnv::new();
    let (stack, _, prs) = submitted(&env);

    // Below a Pull Request merged outside jj-spr.
    env.gh.state().merge_on_github(prs[0]);
    let json = stack.status_json();
    for i in 0..3 {
        assert_eq!(json["entries"][i]["actions"], json!([]), "{json:#}");
    }
    let out = stack.status();
    assert!(!out.text.contains("Submit would"), "{}", out.text);
    assert!(out.flat().contains("has 1 problem."), "{}", out.text);

    // Tips out of line.
    let env = TestEnv::new();
    let (stack, _, _) = submitted(&env);
    let mut file = stack.read();
    file["prs"].as_array_mut().unwrap().swap(0, 1);
    stack.write(file);
    let json = stack.status_json();
    for i in 1..3 {
        assert_eq!(json["entries"][i]["actions"], json!([]), "{json:#}");
    }
}

#[test]
fn submit_refuses_a_closed_pull_request() {
    let env = TestEnv::new();
    let (stack, _, prs) = submitted(&env);
    env.gh.state().close_on_github(prs[1]);
    let before = heads(&env, &prs);
    let out = stack.try_spr("submit", &["-m", "unused"]);
    assert!(!out.success, "submit should refuse:\n{}", out.text);
    assert!(out.flat().contains("is closed"), "{}", out.text);
    nothing_pushed(&env, &prs, &before);
}

#[test]
fn submit_refuses_a_stacked_pull_request_without_an_entry() {
    let env = TestEnv::new();
    let (stack, _, prs) = submitted(&env);
    let before = heads(&env, &prs);
    let mut file = stack.read();
    file["prs"].as_array_mut().unwrap().remove(1);
    stack.write(file);
    let out = stack.try_spr("submit", &["-m", "unused"]);
    assert!(!out.success, "submit should refuse:\n{}", out.text);
    let text = out.flat();
    assert!(
        text.contains(&format!("#{}, which is not", prs[1])),
        "{}",
        out.text
    );
    assert!(text.contains("jj spr close"), "{}", out.text);
    nothing_pushed(&env, &prs, &before);
}

#[test]
fn submit_adds_an_entry_in_the_middle() {
    // B starts out as part of C's Pull Request, then gets its own.
    let env = TestEnv::new();
    let a = change(&env, "a");
    let b = change(&env, "b");
    let c = change(&env, "c");
    let stack = Stack::new(&env);
    stack.entries(json!([{"tip": a, "title": "A"}, {"tip": c, "title": "C"}]));
    stack.spr("submit", &["-m", "unused"]);
    let first = stack.prs();
    env.check_pr(
        first[1],
        &env.pr(first[0]).head_ref,
        &["b.txt", "c.txt"],
        &c,
    );
    let before = heads(&env, &first);

    stack.entries(json!([
        stack.entry(0),
        {"tip": b, "title": "B"},
        stack.entry(1),
    ]));
    stack.spr("submit", &["-m", "split off B"]);
    let prs = stack.prs();
    assert_eq!((prs[0], prs[2]), (first[0], first[1]));
    env.check_pr(prs[1], &env.pr(prs[0]).head_ref, &["b.txt"], &b);
    env.check_pr(prs[2], &env.pr(prs[1]).head_ref, &["c.txt"], &c);
    assert!(
        env.is_ancestor(&before[1], &env.head_of(prs[2])),
        "append-only"
    );
    assert_eq!(env.active_stacks(), vec![prs]);
}
