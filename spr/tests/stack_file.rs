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
