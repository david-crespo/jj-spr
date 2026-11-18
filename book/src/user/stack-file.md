# Stack Files

A stack file lists a stack's Pull Requests before any of them exist: where each one ends, and its title and body. You arrange it locally. `jj spr submit` pushes everything. Later runs update the Pull Requests the same way.

jj-spr writes nothing into change descriptions for a stack file. A change is submitted either with a stack file or with `jj spr diff`, not both.

## The File

```json
{
  "prs": [
    { "tip": "diff-paths", "body": "diff-paths.md" },
    { "tip": "extract-app", "body": "extract-app.md" },
    { "tip": "kxqzwmvtnlop", "title": "Run jj and git in the diff's directory" }
  ]
}
```

You write these keys:

- **`prs`** lists the Pull Requests in stack order, bottom first.
- **`tip`** is a local bookmark or a change ID: the Pull Request's top change. It must resolve to exactly one visible change. A Pull Request's changes are every change from its tip down to the tip of the entry below, or down to trunk for the first entry. Changes above the top tip are not submitted.
- **`body`** is the path of a markdown file, relative to the stack file. A leading `# ` heading is the Pull Request's title, and the rest is its body.
- **`title`** is the title when there is no body file or it has no heading. Without a body file, the body is empty.

jj-spr writes these:

- **`pr`** is the Pull Request's number, written once GitHub creates it. An entry without `pr` is not on GitHub yet.
- **`synced`** is a hash of the title and body last pushed.
- **`pushed`** is the commit ID of the tip when `submit` last pushed the branch or found it up to date. Because jj keeps old versions of changes, you can see what changed locally since then, for example with `jj interdiff --from <pushed> --to <tip>`. Comparing with the branch on GitHub doesn't give that, since jj-spr adds update and merge commits there instead of force-pushing.
- **`merged`** is set by `jj spr sync` once the Pull Request has merged. `submit`, `land`, `sync`, and `close --all` skip merged entries.

jj-spr rewrites the file in one layout and keeps keys it doesn't know. A bookmark used as a tip is only a local pointer: jj-spr pushes its own `spr/…` branch, except for a Pull Request that `jj spr stack-file adopt` took over. A new branch's name is the branch prefix and a slug of the title (at most 100 characters, cut after a whole word when that keeps at least half), with a number added if GitHub, the repository, or another entry already has that name.

Don't check the file in, since jj-spr writes to it on every push. Keep it somewhere jj ignores (a directory in `.gitignore`, or outside the repository); otherwise jj records it in the working-copy change. A directory with `stack.json` and the body files side by side works well.

## Commands

Every command takes the path of the stack file.

### `jj spr stack-file init <path> --tips <revset>`

Writes a new stack file with one entry per tip, bottom first. The tips have to be in a line, each an ancestor of the next. An entry's tip is the change's local bookmark when it has exactly one, or its change ID otherwise. Its title is the first line of the change's description. `init` doesn't create body files. It refuses to replace an existing file.

```shell
jj spr stack-file init stack/stack.json --tips 'diff-paths | extract-app | kxqzwmvtnlop'
```

### `jj spr stack-file status <path>`

Shows each entry with its changes, its title and where the title comes from, its Pull Request, and what `submit` would do. It lists every problem `submit` would refuse on, not just the first, and exits with an error when there is one. It leaves out what `submit` would do for an entry when that depends on how a problem is fixed: when the changes of the entry, or of one below it, are not known, or when one of those entries has a Pull Request that has merged or was closed.

For an entry with `pushed`, `status` also shows which changes are new, edited, or only reworded since the `pushed` commit, or that nothing changed locally. A change is matched to its old version by change ID among the ancestors of the `pushed` commit. A change that was only rebased, with the same diff, counts as unchanged. When the `pushed` commit is no longer in the repository, `status` says that what changed is unknown.

`--json` prints the same as JSON. An entry with `pushed` includes that key. Each of its changes has `since_pushed`: `new`, `edited`, `reworded`, `unchanged`, or `unknown`.

There are no commands to add, move, or remove entries: edit the file, then run `status`.

### `jj spr stack-file adopt <path>`

Takes over Pull Requests opened without jj-spr, so that `submit` updates them instead of opening new ones. It is for a stack pushed with `jj git push`, where each Pull Request's branch on GitHub has the name of the local bookmark at the entry's tip.

For each entry without `pr` whose tip is a local bookmark, `adopt` looks for Pull Requests in this repository (not from forks) whose branch has the bookmark's name. When there is none, when they are all closed or merged, or when more than one is open, `adopt` says so and leaves the entry alone. For an entry without an open Pull Request, the next `submit` opens one on a `spr/…` branch. When exactly one is open, `adopt` writes:

- `pr`, the Pull Request's number.
- `pushed`, the commit of the remote-tracking bookmark, which is what was last pushed. Without a remote-tracking bookmark, the entry gets no `pushed`.
- `synced`, the hash of the entry's current title and body. `adopt` changes nothing on GitHub. Since `synced` has the local text, `submit` keeps GitHub's title and body where they differ, as for a Pull Request edited on GitHub. `adopt` says for each entry whether the two match. With `--copy-github-text`, it writes GitHub's title and body into the entry's body file instead, so that they match.

Then it stops tracking the bookmark's remote branch (`jj bookmark untrack`). Since `submit` adds commits to that branch from then on, a tracked bookmark would move to those commits, or become conflicted, on the next fetch. By default, jj treats the changes on an untracked remote branch as immutable, so `adopt` rewrites them with the same content (with `jj metaedit --force-rewrite --ignore-immutable`) to keep them editable.

`--dry-run` shows what `adopt` would do. Running `adopt` again leaves the entries it adopted as they are.

When the bookmarks were pushed before `adopt`, `submit` pushes nothing until the changes are edited. Then it adds commits to the existing branches without a force push, as for a Pull Request it opened. Those commits sit on top of the commits that were on the branch before `adopt` rewrote them, so jj shows the old versions next to the rewritten ones, with the same change IDs. The changes stay divergent until the Pull Request lands or is closed and its branch is deleted. Until then, jj refuses their change IDs alone, so refer to them by bookmark or commit ID.

`land` and `close` delete an adopted branch, as they do jj-spr's own. `jj spr cleanup` only looks at branches with jj-spr's prefix, so it never deletes an adopted branch.

### `jj spr submit <path>`

Use it for the first push and for every later one. It:

1. Works out each entry's changes from the tips.
2. Checks everything before changing anything on GitHub (see [Refusals](#refusals)).
3. For each entry, bottom first: pushes its branch, creates the Pull Request if there is none, and writes `pr`, `synced`, and `pushed` to the file. If it fails partway, the entries whose Pull Requests were opened have their `pr` in the file. Running `submit` again opens the rest. If creating a new Pull Request fails after its branch was pushed, the file has no record of that branch, so the rerun pushes the entry to a new branch. `jj spr cleanup --confirm` deletes the branch left over from the failed run.
4. For an existing Pull Request, updates the title and body on GitHub when they changed in the file since the last submit, and updates `synced`.
5. Creates or rebuilds the GitHub stack.

Pushes are append-only, as with `jj spr diff`: a new commit for each new change at the top of a Pull Request, one update commit when an earlier change was edited, and a merge of the branch below when that branch moved. Each change of a new Pull Request becomes a commit of its own.

When a title or body was edited on GitHub and not in the file, GitHub's version stays. `status` and `submit` show when this happens. When both changed, `submit` refuses; `--overwrite-github` replaces GitHub's version with the file's.

Options:

- `--dry-run` shows what would happen without pushing, opening Pull Requests, or writing the file.
- `--upto <tip>` submits the entries up to and including the one with that tip. The entries above it are not checked, so an unfinished entry doesn't block the ones below it.
- `-m <message>` is the message for the commits that update existing Pull Requests. Without it, `submit` asks.
- `--draft` opens new Pull Requests as drafts.
- `--discard-remote-changes` pushes even when someone else pushed to a Pull Request.

### `jj spr land <path>`

Lands the stack, or with `--upto <tip>`, the entries up to and including that one. It lands the Pull Requests one at a time, bottom first. Each one is squash-merged on its own, so each commit on the main branch has its Pull Request's title and body. In a GitHub stack, the Pull Requests below the highest one being landed have to match the file's entries below it.

### `jj spr sync <path>`

Cleans up after Pull Requests merge: it abandons the merged changes, rebases the rest onto the main branch, and marks the merged entries `merged`.

### `jj spr close <path> --pr <tip>` and `jj spr close <path> --all`

`--pr` closes one entry's Pull Request. `--all` closes all of them, top first. The entries stay, with `pr`, `synced`, and `pushed` removed, so a later `submit` opens new Pull Requests. To drop a Pull Request from the stack, close it, then remove its entry (and its changes, if you don't want them in the entry above).

## Refusals

`submit` stops before changing anything on GitHub when:

1. A tip is missing, hidden, divergent, a conflicted or remote bookmark, relative to the working copy (like `@-`), or neither a local bookmark nor a change ID.
2. The tips are not in a line, an entry's changes are not in a line, two entries have the same tip, or an entry has no changes.
3. A change is conflicted or has no description, a body file can't be read, or an entry has no title.
4. A change has a `Pull Request:` line. Stack files and `jj spr diff` are not used on the same changes.
5. An entry's Pull Request has merged. Run `jj spr sync <path>` first.
6. An entry's Pull Request was closed. Either remove the entry, so its changes become part of the entry above it (for the top entry, they are no longer submitted), or remove its `pr` and `synced` keys, so the next `submit` opens a new Pull Request for it on a new branch.
7. An open Pull Request in the GitHub stack has no entry. Put the entry back, or close the Pull Request.
8. A title or body changed both in the file and on GitHub since the last submit. `--overwrite-github` replaces GitHub's.

Entries can be inserted, removed, and reordered. After a reorder, `submit` rebuilds the GitHub stack, and a Pull Request that moved up gets its whole change in a new commit.

## Example

```shell
# Three Pull Requests: a bookmark at each top change.
jj spr stack-file init stack/stack.json --tips 'routes | handlers | docs'

# Write the bodies, and point the entries at them.
$EDITOR stack/routes.md stack/handlers.md stack/stack.json
jj spr stack-file status stack/stack.json

jj spr submit stack/stack.json

# After review: edit the changes, then push the updates.
jj spr submit stack/stack.json -m 'Review comments'

# Land everything, then clean up.
jj spr land stack/stack.json
jj spr sync stack/stack.json
```
