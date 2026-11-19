---
name: jj-spr
description: "Open, update, land, and clean up a stack of GitHub pull requests from jj changes with jj-spr stack files: `jj spr stack-file init`, `status`, and `adopt`, and `jj spr submit`, `land`, `sync`, and `close`. Use when the user wants a stack of PRs pushed, updated, landed, or closed with jj-spr, when a jj-spr command refuses, or when taking over a stack that was pushed with `jj git push`."
---

# Stacks of PRs with jj-spr stack files

A stack file is a JSON file that lists a stack's pull requests, bottom
first, with each PR's title and body. `jj spr submit` opens or updates the
PRs on GitHub from it. `docs/user/stack-file.md` in the jj-spr repository
(two directories up from this skill) is the full reference for the format,
every flag, and every refusal. Each command's `--help` has its flags.

## Before you start

- Check that `jj spr stack-file --help` works. The stack-file commands exist
  only in the user's fork of jj-spr.
- When a command says the branch prefix or the GitHub token is not
  configured, ask the user to run `jj spr init`. It is interactive, so never
  run it yourself.
- A change belongs to a stack file or to `jj spr diff`, never both. Never
  run `jj spr diff` or `jj git push` on changes that are in a stack file.
- Run `submit`, `land`, `close`, and `adopt` only when the user asks for
  them. `status`, `submit --dry-run`, and `adopt --dry-run` change nothing
  and are safe at any time. `submit --dry-run` says it would register a
  GitHub stack even when the stack already exists.

## The file

Keep a stack in a directory of its own, with the body files next to the
stack file: `.claude/notes/<YYYY-MM-DD>-<slug>-stack/stack.json`. jj-spr
rewrites the file on every push, so it must not be checked in. jj records
every file it doesn't ignore in the working-copy change, so check that the
directory is ignored (`jj status` doesn't list the file).

```json
{
  "prs": [
    { "tip": "routes", "body": "routes.md" },
    { "tip": "handlers", "body": "handlers.md" },
    { "tip": "kxqzwmvtnlop", "title": "Document the handlers" }
  ]
}
```

- `tip` is a local bookmark or a change ID for the PR's top change. The PR's
  changes run from its tip down to the tip below it, or to trunk.
- `body` is a markdown file, relative to the stack file's directory. Its
  `# ` heading on line 1 is the PR's title, and the rest is the PR's
  description. `title` is for an entry without a body file.
- jj-spr writes `pr`, `synced`, `pushed`, and `merged`. Don't add or edit
  them, except where a refusal's message says to remove one, or to list PRs
  that merged before the file existed (below).
- Leave keys you don't recognize in place. Other tools add their own, and
  jj-spr keeps them.

## Starting a stack

1. Give every change in the stack a description. Put a bookmark on each
   PR's top change, or note its change ID.
2. Write the file from the tips:

   ```sh
   jj spr stack-file init .claude/notes/2026-11-19-routes-stack/stack.json --tips 'routes | handlers | kxqzwmvtnlop'
   ```

3. For each entry, write a body file and set `body`, or keep the `title`
   that `init` wrote.
4. Run `jj spr stack-file status <file>` and fix every problem it lists.
5. When the user says to push, run `jj spr submit <file>`.

When some of the stack's PRs merged before the stack file existed, such as
a stack moving to a stack file partway through, list them at the bottom so
the page shows the whole stack. Give each entry its `tip`, `body` or
`title`, `pr`, and `"merged": true`. Every command skips merged entries, so
the tip doesn't have to resolve. Mark only PRs that GitHub shows as merged:
`submit` and `land` skip an entry marked merged, so an open PR marked that
way is never pushed or landed.

## Changing a stack

There are no commands to add, move, or remove entries. Edit the JSON. After
any change to the commits or the file, run `status`. It shows each PR's
changes, what changed since the last push, and what `submit` would do.

Pass `-m '<message>'` to `submit` whenever a PR that exists will get new
commits. Without it, `submit` asks at the terminal and hangs. The message is
for the update commits, so say what changed since the last push.

To push the lower PRs only, pass `--upto <tip>`. The entries above it are
not checked or pushed, and their PRs stay as they are.

- **Edit a change.** Edit it in jj as usual. `submit` adds one update commit
  to that PR's branch and a merge commit to each PR above it. It never
  force-pushes.
- **Add a change to a PR.** A change inserted below the tip is in the PR
  already. For one at the top, use `jj new --insert-after <tip>` so the PRs
  above are rebased onto it. jj leaves the bookmark where it was, so move it
  (`jj bookmark move <name> --to <rev>`) or put the new change ID in `tip`.
  Otherwise the new change is in the PR above.
- **Add a PR.** Add an entry with `tip` and `body` or `title` at its place
  in `prs`.
- **Reorder PRs.** Reorder the changes in jj and the entries in the file to
  match.
- **Change a title or body.** Edit the body file or `title`. `submit`
  updates GitHub. A title or body edited only on GitHub is kept. If both
  changed, `submit` refuses. `--overwrite-github` replaces GitHub's version
  with the file's, so ask the user before passing it.
- **Drop a PR.** If the PR is open on GitHub, run
  `jj spr close <file> --pr <tip>` first, with the user's approval. Then
  remove the entry. Abandon its changes after that, unless they should
  become part of the PR above. Abandoning them first leaves `close` with a
  tip it can't find.

## Landing

```sh
jj spr land <file> --upto <tip>
jj spr sync <file>
```

`land` squash-merges the PRs one at a time, bottom first, so each commit on
the main branch has its PR's title and body. Without `--upto` it lands the
whole stack. Give it a long timeout: after each merge, GitHub can take a
minute to work out whether the next PR can merge, and `land` waits. It does
not wait for CI. If GitHub blocks a PR for required checks or reviews,
`land` stops there with an error. If any PRs landed, run `sync`. Tell the
user which PR is blocked, and run `land` again once they say the checks or
reviews are done.

Always run `sync` after `land`. It abandons the merged changes, rebases the
rest onto the main branch, and marks the merged entries. A PR that merged on
GitHub some other way needs `sync` too, and `submit` refuses until it has
run. When every PR in the stack has merged, `sync` leaves the working copy
on the old main commit, so run `jj new 'trunk()'`.

## Taking over a stack pushed with `jj git push`

`jj spr stack-file adopt <file>` takes over open PRs whose branch has the
same name as the bookmark at an entry's tip. It changes nothing on GitHub.

1. Write the file with `jj spr stack-file init`, with each PR's bookmark as
   its tip.
2. Run `jj spr stack-file adopt <file> --dry-run` and show the user the
   output.
3. Tell the user what `adopt` does locally, and run it without `--dry-run`
   once they agree:
   - It untracks the bookmarks from their remote branches and rewrites the
     adopted changes so they stay editable.
   - After the first `submit` that adds commits to an adopted branch, jj
     shows each adopted change as divergent until its PR lands or is closed.

Keep bookmarks as the tips of adopted entries. Once a change is divergent,
jj refuses its change ID, so you and the user both have to refer to it by
bookmark or commit ID.

## When a command refuses

`status` lists every problem at once, and each message says how to fix it.
These need care or the user's decision:

- **A tip doesn't resolve to one visible change.** The bookmark was
  deleted, the change was abandoned, or `tip` is the change ID of an adopted
  change that became divergent. Set `tip` to a bookmark or to the ID of one
  visible change.
- **A PR was closed.** Ask the user whether to remove the entry or open a
  new PR for it. For a new PR, remove the keys the message lists.
- **An open PR in the GitHub stack has no entry.** Put the entry back, or
  close that PR with the user's approval.
- **A PR has commits that are not in the local change.** Someone else may
  have pushed to the branch. Look at the PR's commits on GitHub first. Pass
  `--discard-remote-changes` only with the user's approval.
