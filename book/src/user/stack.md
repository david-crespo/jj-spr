# Stack Multiple PRs

The differences between jj-spr's commit-based workflow and GitHub's default branch-based workflow are most apparent when you have multiple reviews in flight at the same time.

This guide assumes you're already familiar with the workflow for [simple, non-stacked PRs](./simple.md).

## Two Approaches: Independent vs Dependent

When working with multiple PRs, you have two main approaches:

1. **Independent changes with `--cherry-pick`** (recommended for most cases)
   - Changes can be landed in any order
   - Simpler workflow, fewer manual steps
   - Best when changes don't strictly depend on each other
   - Example: Bug fix + new feature in same codebase area

2. **Dependent stacks** (for true dependencies)
   - Changes must be landed in order (parent → child)
   - More complex workflow
   - Only use when second change literally won't work without first
   - Example: Add database table → Add API endpoints using that table

**Not sure which to use?** Start with `--cherry-pick`. You can always switch to dependent stacks if needed.

In Jujutsu, managing stacked changes is much simpler than in Git because Jujutsu maintains stable change IDs and automatically handles rebasing operations.

## Independent Changes with --cherry-pick (Recommended)

**For most users, this is the workflow you want.** It's simpler and more flexible than dependent stacks.

### Creating Independent Changes

1. Create your first change:
   ```shell
   jj new main@origin
   # Make changes...
   jj describe -m "Add authentication module"
   jj new  # Move to empty working copy, PR change at @-
   jj spr diff  # Create PR #123
   ```

2. Create your second change (also based on main):
   ```shell
   jj new main@origin
   # Make changes...
   jj describe -m "Fix login bug"
   jj new  # Move to empty working copy, PR change at @-
   jj spr diff --cherry-pick  # Create PR #124 as independent
   ```

The `--cherry-pick` flag tells jj-spr to create the PR as if the change were based directly on `main@origin`, even if locally you have both changes in a stack.

### Landing Independent Changes

You can land these in **any order:**

```shell
# Land either one first
jj spr land --cherry-pick -r <change-id>

# Then the other
jj spr land --cherry-pick -r <other-change-id>
```

**After landing, bring your local changes up to date:**
```shell
jj spr sync
```

**Benefits:**
- ✅ Land in any order
- ✅ Simpler workflow
- ✅ Less chance of breaking your stack
- ✅ Reviewers see clean, focused PRs

## Dependent Stacks (Advanced)

**Only use this if your changes truly depend on each other.** The workflow is more complex.

### Creating Dependent Changes

This is for when the second change literally won't work without the first.

1. Create your first change on top of `main`:
   ```shell
   jj new main@origin
   # Make changes...
   jj describe -m "Add authentication module"
   ```

2. Create your second change on top of the first:
   ```shell
   jj new
   # Make changes that depend on the authentication module...
   jj describe -m "Add user profile endpoints"
   ```

3. Create an empty working copy on top:
   ```shell
   jj new
   ```

   Now your stack looks like:
   - `@` = empty working copy
   - `@-` = "Add user profile endpoints" (second PR change)
   - `@--` = "Add authentication module" (first PR change)

4. Run `jj spr diff --all` to create PRs for all changes in your stack:
   ```shell
   jj spr diff --all
   ```

   This is equivalent to calling `jj spr diff` on each change in your stack from `@-` back to `main@origin`.

## GitHub Stacks

A dependent PR targets the PR branch of the change below it. When the
repository has stacked pull requests enabled, jj-spr registers the chain as a
GitHub
[stack](https://docs.github.com/en/pull-requests/how-tos/stacked-pull-requests),
which GitHub shows as one unit in its UI.

- Updates add commits to the PR branch (with your `-m` message) and never
  rewrite it. An update merges in the parent PR's current head, so GitHub
  keeps showing only the change itself, and keeps the change in ordinary
  commits, which GitHub replays when it rebases the stack after a merge. After
  a reorder, a PR that moved up gets its whole change in a new commit.
- After publishing, `jj spr diff` registers the chain as a GitHub stack, or
  appends new PRs to the existing one. GitHub does not allow changing the base
  of a stacked PR, so when you reorder, insert, or remove changes, `jj spr diff
  --all` dissolves the stack first and rebuilds it. It refuses to do that if
  the stack contains PRs the run would not update; run it from the top of the
  stack instead.
- Without `--all`, a change stacks on its parent's existing PR, so the parent
  must have been submitted already.
- `jj spr land` merges through GitHub's stack-aware merge. Landing the bottom
  PR merges just that one; landing a PR higher up merges it together with
  every PR below it, after listing them and asking (pass `--yes` to skip the
  question, which is required outside a terminal). Every PR that would merge
  is checked first: none may be a draft, and each must be approved if
  `spr.requireApproval` is set. How `land` merges depends on the repository's
  default commit message for squash merges:
  - When the default is the pull request title and description (`PR_TITLE`
    and `PR_BODY` in GitHub's API), `land` makes one merge of the highest PR
    being landed. GitHub merges any PRs below it in the same operation, with
    no rebase in between. Each commit on the main branch has its PR's title,
    followed by its number, and its description.
  - With any other default, or if jj-spr can't read it, `land` merges the PRs
    one at a time, bottom first, so each commit gets jj-spr's title and
    message. After each merge, GitHub retargets the next PR to the main branch
    and rebases the rest of the stack. `land` waits for that rebase before it
    merges the next PR. When it merges more than one PR this way, `land`
    prints a hint about changing the default.

  Afterwards, run `jj spr sync` and then `jj spr diff --all`. `land` does not
  wait for CI. If GitHub blocks a PR for required checks or reviews, `land`
  stops there and prints which PRs landed. Run `jj spr sync`, then run
  `jj spr land` again once GitHub allows the merge.
- If stacked pull requests are not enabled for the repository, the PRs still
  target each other; they just aren't registered as a GitHub stack.

**Upgrading from an older jj-spr:** Older versions created a base branch of
its own (`<prefix>main.<title>`) for each dependent PR unless `spr.nativeStacks`
was set. The `--native-stack` and `--no-native-stack` flags are removed, and
`spr.nativeStacks` is ignored if still set. The next `jj spr diff` retargets a
PR on such a base branch to its parent's PR branch or to the main branch.
Until then, `jj spr land` refuses to land it. Once no open PR uses the old base
branches, `jj spr cleanup --confirm` deletes them from GitHub.

Limitations: repositories that merge through a merge queue are not supported
yet.

## Understanding Your Stack

Use `jj log` to visualize your stack:
```shell
jj log -r 'main@origin..'
```

**Example output:**
```
@  qpvuntsm you@example.com 2024-01-15 12:00:00
│  (empty)
○  kmkuslkw you@example.com 2024-01-15 11:30:00
│  Add user profile endpoints
○  rlvkpnrw you@example.com 2024-01-15 11:00:00
│  Add authentication module
◆  main@origin
```

In this example:
- `@` = empty working copy
- `kmkuslkw` = second change (depends on first)
- `rlvkpnrw` = first change (base of stack)
- Change IDs are in the first column

This shows all your changes that are descendants of `main@origin`.

### Visual: Local Stack vs GitHub PRs

Here's what the above stack looks like locally vs on GitHub:

```
Local Jujutsu State:                 GitHub State:

@  qpvuntsm (empty)
│
○  kmkuslkw                     →    PR #124: "Add user profile endpoints"
│  Add user profile endpoints        base: spr/alice/add-authentication-module (PR #123's branch)
│                                    branch: spr/alice/add-user-profile-endpoints
○  rlvkpnrw                     →    PR #123: "Add authentication module"
│  Add authentication module         base: main
│                                    branch: spr/alice/add-authentication-module
◆  main@origin
```

**Key points:**
- Each change has a unique ID (`rlvkpnrw`, `kmkuslkw`)
- jj-spr creates GitHub branches automatically, named with the branch prefix and the PR's title (`spr/alice/add-user-profile-endpoints`)
- Stacked PRs: PR #124 is based on PR #123's branch
- When PR #123 lands, PR #124 automatically updates to be based on `main`

## Updating Changes in the Stack

Suppose you need to update the first change (authentication module with ID `rlvkpnrw`) in response to review feedback.

**Step 1: Find the change ID**

First, identify which change you want to edit:
```shell
jj log -r 'main@origin..'
```

Output:
```
@  qpvuntsm (empty)
○  kmkuslkw Add user profile endpoints
○  rlvkpnrw Add authentication module  ← This is the one we want to update
◆  main@origin
```

**Method 1: Squash from working copy (Recommended)**

This is the recommended approach that aligns with the standard workflow:

1. Make your changes in your working copy (`@`):
   ```shell
   # You're already at @ (empty working copy)
   # Make your edits...
   ```

2. Squash the changes into the target change:
   ```shell
   jj squash --into rlvkpnrw  # Use the actual change ID
   ```

3. Update the PR:
   ```shell
   jj spr diff -r rlvkpnrw
   ```

**Method 2: Direct editing (jj edit)**

Alternatively, edit the change directly:

1. Edit the change directly:
   ```shell
   jj edit rlvkpnrw  # Use the actual change ID
   # Make your changes...
   ```

2. The changes are automatically absorbed. Jujutsu will automatically rebase descendant changes.

3. Update the PR for that specific change:
   ```shell
   jj spr diff -r @  # @ is now rlvkpnrw since we edited it
   ```

4. Return to your empty working copy:
   ```shell
   jj new kmkuslkw  # Creates new working copy on top of the stack
   ```

**Which method to use?**
- Use `jj squash` (Method 1) for consistency with the standard workflow
- Use `jj edit` (Method 2) if you want to work directly on the change

## Landing Stacked Changes

> 🚨 **CRITICAL WARNINGS FOR STACKED LANDING:**
>
> 1. **Landing order matters:** Changes land in order (parent before child). In a GitHub stack, landing a child also merges every PR below it, after asking. Outside a GitHub stack, `jj spr land` refuses to land a PR while the PR below it is open.
>
> 2. **Sync after landing:** After `jj spr land` (or a merge on GitHub), run `jj spr sync` to abandon the landed change and rebase the rest of the stack. See [Syncing after merges](#syncing-after-merges).

### Landing Process (Parent Change)

Using our example stack where `rlvkpnrw` (auth module) is the parent and `kmkuslkw` (user profiles) is the child:

```
Before landing:
○  kmkuslkw  Add user profile endpoints (PR #124)
○  rlvkpnrw  Add authentication module (PR #123)  ← Land this first
◆  main@origin
```

**Step-by-step:**

1. **Land the parent change:**
   ```shell
   jj spr land -r rlvkpnrw  # Use the actual change ID
   ```

2. **Sync the local stack:**
   ```shell
   jj spr sync  # abandons rlvkpnrw and rebases kmkuslkw onto main@origin
   ```

3. **Update remaining PRs:**
   ```shell
   jj spr diff --all  # Updates PR #124 from the rebased change
   ```

**After landing:**
```
○  kmkuslkw  Add user profile endpoints (PR #124, now based on main)
◆  main@origin (now includes rlvkpnrw)
```

### Checking a stack

`jj spr stack` shows the local stack (top first, like `jj log`) next to each
change's Pull Request: its state, CI checks, review and base branch, and the
GitHub stack they belong to:

```
  🧱  GitHub stack #1: #1, #2, #3 (bottom first)
○  ryxopsnk  Change C  #3  open, needs update  CI ✗  changes requested, 1 thread  → spr/change-b
○  nrlzpwsu  Change B  #2  open                CI ●  no review                    → spr/change-a
○  xrrqyttw  Change A  #1  open                CI ✓  approved                     → main
◆  main@origin

  ⚠️  The local change for #3 differs from the Pull Request. Run `jj
      spr diff --all` to update it.
  ⚠️  #3 has failing checks: build.
  ⚠️  #3 has changes requested.
  ⚠️  #3 has 1 unresolved review thread.
  🛬  #1 is ready to land: `jj spr land -r xrrqyttw`
```

`CI ✓` means every check passed, `✗` that one failed, `●` that some are still
running, and `-` that there are none. Checks belong to a Pull Request's
current head, so they reset when you push.

It points out anything that does not match, with the command that fixes it:
changes without a Pull Request, local changes that differ from their Pull
Request, drafts, failing checks, requested changes, unresolved review threads,
merged or closed Pull Requests, Pull Requests that target the wrong branch,
and ones missing from the GitHub stack. The last line
names the Pull Requests, from the bottom of the stack up, that are ready to
land: up to date, not drafts, checks passed, no unresolved threads, and
approved (if `spr.requireApproval` is set, or the repository requires
reviews). In a GitHub stack, landing the top one of those lands them all.

`-v` replaces the CI and review columns with lines under each Pull Request:
its checks by name, who approved or requested changes, who has yet to review,
and the unresolved threads:

```
○  ryxopsnk  Change C  #3  open  → spr/change-b
│    checks   ✗ build   ✓ clippy
│    review   changes requested by @bob
│    threads  ▸ src/c.rs:12  @bob  "Please rename this"
```

`--json` prints the same information for scripts, bottom first: the changes
with their Pull Requests, the GitHub stacks, the findings (each with a stable
`kind`, such as `failing_checks` or `needs_update`), and `ready_to_land`.

It only reads from GitHub and the repository, and changes nothing. Use `-r` to
pick another stack.

### Syncing after merges

`jj spr sync` brings a local stack up to date after some of its Pull Requests
merged, whether with `jj spr land` or on GitHub:

1. It fetches, and looks up the Pull Request of every change between
   `main@origin` and `@-` (or the revision given with `-r`).
2. It abandons the changes whose Pull Requests merged. jj moves anything above
   an abandoned change onto its parent, so this also works when a PR in the
   middle merged first (for example a cherry-picked one).
3. It rebases the rest of the stack onto `main@origin`.

It stops before changing anything if a merged change has local edits that were
not in the merged Pull Request, so unpublished work is never abandoned. It
never rebases just because `main` moved (use `jj rebase` for that), and it
reports conflicts left by the rebase instead of submitting them. `--dry-run`
shows what it would do, and `jj undo` reverts a sync.

Afterwards, run `jj spr diff --all` to update the remaining Pull Requests.

### Working on someone else's stack

`jj spr patch <number>` creates local changes from existing Pull Requests.
If the Pull Request is part of a native GitHub stack, it fetches every open
Pull Request in the stack, bottom first, whichever one you name:

```
$ jj spr patch 2
  🔄  Fetching from origin
  🧱  #2 is part of a stack: #1, #2, #3 (bottom first)
  📥  #1: created yuynpkkv Change A
  📥  #2: created pkmlqutt Change B
  📥  #3: created yxpnkqnw Change C
  👉  Working copy is now on top of yxpnkqnw
```

Each change holds its Pull Request's own diff and a `Pull Request:` trailer,
so `jj spr diff --all` updates those Pull Requests instead of opening new
ones. `--no-stack` fetches only the named Pull Request, and `--no-checkout`
leaves the working copy where it is.

When someone pushes to one of the Pull Requests, run `jj spr patch` again to
update your changes. Each Pull Request that already has a local change is
compared with it:

- **up to date:** nothing to do
- **updated from GitHub:** GitHub has commits the change lacks, and the
  change has nothing that is not on GitHub
- **kept:** the change has local edits that are not on GitHub yet, and GitHub
  has nothing new; `jj spr diff` pushes them

If a change and its Pull Request both have edits the other lacks, it changes
nothing and says which. Move your edits into a separate change (for example
with `jj split`) and run it again.

`jj spr diff` never pushes over someone else's commits either. If a Pull
Request has commits its local change lacks, it stops before pushing anything
and suggests `jj spr patch`. Your own edits since your last push don't count,
and neither does GitHub rebasing a stack after a merge. To replace what is on
GitHub with your version anyway, pass `--discard-remote-changes`. The other
person's commits stay in the branch's history, but their edits disappear
from the Pull Request's diff.

### Best Practices

- ✅ **Always land in order:** Parent → Child → Grandchild
- ✅ **Double-check change IDs** before landing (use `jj log`)
- ✅ **If changes aren't truly dependent, use `--cherry-pick` instead** (see above)

## Rebasing the Whole Stack

One of the major advantages of Jujutsu is that rebasing your entire stack onto new upstream changes is trivial:

1. Fetch the latest changes:
   ```shell
   jj git fetch
   ```

2. Rebase your stack:
   ```shell
   jj rebase -s <root-change-id> -d main@origin
   ```

   Where `<root-change-id>` is the first change in your stack.

3. Update all PRs:
   ```shell
   jj spr diff --all
   ```

## One Pull Request from Several Changes

A Pull Request can be made of several adjacent changes. Each one gets the
same `Pull Request:` line, and the Pull Request branch has a commit for each
change, so reviewers can read it a step at a time.

```shell
# Open one PR from the three changes; each keeps its own description locally
jj spr diff -r 'xyz::abc' --one-pr

# Add new changes on top of it later
jj spr diff -r 'xyz::@-' --one-pr
```

The Pull Request's title and message come from its top change. Selecting any
of its changes (with `-r`, or as the end of a range) selects all of them, and
`land`, `close`, `sync`, and `stack` treat them as one.

To give the Pull Request a title or body of its own, pass `--title` or
`--body`:

```shell
jj spr diff -r 'xyz::abc' --one-pr --title "Extract the HTTP app" --body "$(cat body.md)"
```

They set the title and body on GitHub, for a new Pull Request or an existing
one, and leave every local description as it is. Whichever one you leave out
comes from the top change. They apply to one Pull Request per run,
so a run that would submit several refuses them: run `jj spr diff` once for
each. Later runs without them warn that the title and message differ from
the descriptions; `--update-message` there would replace yours with the top
change's.

Splitting a submitted change with `jj split` keeps the `Pull Request:` line on
both halves, so the halves become one Pull Request without `--one-pr`.

When you edit a change below the top, the next `jj spr diff` adds one commit
with the result, as it does for a Pull Request of one change.

To open a stack of such Pull Requests in one run, pass a revset of the top
change of each to `--tips`:

```shell
# Nine changes in a line: a1 a2 a3 b1 b2 b3 c1 c2 c3
jj spr diff --tips 'a3 | b3 | c3'
```

This opens three Pull Requests (a1 to a3, b1 to b3, and c1 to c3), stacked
on each other. `--tips` submits every change from trunk (or `--base`) up to
the topmost tip, and cuts that range into one Pull Request per tip. The tips
have to be in a line. Changes above the topmost tip are not submitted.

`--tips` neither splits a Pull Request nor joins two. If the range already
has Pull Requests, a tip can't be a change below the top of one, and each of
them needs a tip of its own: its top change, or a new change above it. A new
change goes into the Pull Request of the first tip at or above it, so with a4
added above a3,
`--tips 'a4 | b3 | c3'` adds a4 to the first Pull Request. Afterward, every
submitted change has a `Pull Request:` line, and later updates only need
`jj spr diff --all`.

## Working with Revsets

Jujutsu's revset language makes it easy to work with stacks:

```shell
# Show all your changes not yet in main
jj log -r 'mine() & ~main@origin'

# Create or update PRs for every change from main up to @-
jj spr diff -r 'main@origin..@-'

# Show changes that have PRs
jj log -r 'description(substring:"Pull Request:")'
```

## Tips for Stack Management

1. **Keep changes focused**: Each change should represent one logical unit of work.

2. **Use descriptive commit messages**: This helps when navigating your stack.

3. **Leverage change IDs**: Unlike Git commits, Jujutsu change IDs remain stable through rebases.

4. **Use `jj split` when needed**: If a change gets too large, split it:
   ```shell
   jj split -r <change-id>
   ```

5. **Monitor your stack**: Regularly run `jj log` to understand your stack's structure.

The Jujutsu + jj-spr workflow makes stacked PRs feel natural and eliminates much of the complexity found in traditional Git-based stacking workflows.
