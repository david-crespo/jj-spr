# Commands Reference

This page provides a complete reference for all SPR commands.

## Global Options

All commands support the following global options:

- `-h, --help` - Show help information
- `-V, --version` - Show version information
- `--github-auth-token <TOKEN>` - GitHub token (default: `spr.githubAuthToken`)
- `--github-repository <OWNER/REPO>` - GitHub repository (default: `spr.githubRepository`)
- `--branch-prefix <PREFIX>` - Prefix for new pull request branches (default: `spr.branchPrefix`)
## Commands

### `jj spr init`

Initialize SPR in the current repository. This command prompts for your GitHub Personal Access Token and configures the repository.

**Usage:**
```bash
jj spr init
```

**What it does:**
- Detects GitHub repository from git remotes
- Offers to use the GitHub CLI's token if `gh` is logged in; otherwise prompts for a GitHub Personal Access Token
- Stores configuration in the repository's jj config (`jj config set --repo`)

---

### `jj spr diff`

Create or update a pull request for one or more changes.

**Usage:**
```bash
jj spr diff [OPTIONS]
```

**Options:**
- `-r, --revision <REV>` - Revision(s) to operate on (default: `@-`)
  - Single revision: `-r @-`, `-r <change-id>`
  - Range: `-r main..@`, `-r a::c`
- `-a, --all` - Create/update PRs for all changes from base to current
- `--base <REV>` - Base revision for `--all` mode (default: trunk)
- `-m, --message <MSG>` - Message for PR update commits
- `--update-message` - Update PR title/description from local commit
- `--title <TEXT>`, `--body <TEXT>` - Title or body for the PR on GitHub, in place of the top change's (only for a run with one PR)
- `--draft` - Create PR as draft
- `--cherry-pick` - Create PR as if cherry-picked onto main
- `--no-cherry-pick` - Remove the `Cherry Pick:` marker from the commit description
- `--one-pr` - Submit the selected range as one PR, with a commit for each change
- `--tips <REVSET>` - Submit one PR per tip, each made of the changes above the tip below it
- `--discard-remote-changes` - Push even if someone else pushed commits to the PR
- `--dry-run` - Show what would happen without pushing or creating PRs

See [One Pull Request from Several Changes](../user/stack.md#one-pull-request-from-several-changes) for `--one-pr`, `--tips`, `--title`, and `--body`.

**Examples:**
```bash
# Create PR for parent of working copy (default)
jj spr diff

# Create PR for specific change
jj spr diff -r <change-id>

# Create PRs for all changes in range
jj spr diff -r main..@

# Create independent PR
jj spr diff --cherry-pick

# Update PR with new changes and message
jj spr diff -m "Address review comments"
```

---

### `jj spr land`

Land (squash-merge) an approved pull request.

**Usage:**
```bash
jj spr land [OPTIONS]
```

**Options:**
- `-r, --revision <REV>` - Revision to land (default: `@`)
- `--cherry-pick` - Land PR independently (for use with stacks)
- `-y, --yes` - Merge the PRs below the chosen one in its GitHub stack without asking (required outside a terminal)
- `[STACK_FILE]`, `--upto <TIP>` - Land a stack file's PRs, or only those up to the entry with this tip

**Examples:**
```bash
# Land PR for parent of working copy
jj spr land -r @-

# Land specific change
jj spr land -r <change-id>

# Land independently (with --cherry-pick)
jj spr land --cherry-pick -r <change-id>
```

In a GitHub stack, landing a PR also merges every PR below it. When the repository's default commit message for squash merges is the PR title and description, `land` does that in one merge. Otherwise it merges the PRs one at a time, bottom first. `land` does not wait for CI. If GitHub blocks a PR for required checks or reviews, `land` stops there. See [GitHub Stacks](../user/stack.md#github-stacks).

**After landing**, update your local stack:
```bash
jj spr sync
```

---

### `jj spr list`

List your open pull requests in the repository and their review status.

**Usage:**
```bash
jj spr list
```

**Output includes:**
- PR title and URL
- Review status (accepted, changes requested, or pending)

For CI checks and the state of each PR in your local stack, use `jj spr stack`.

---

### `jj spr close`

Close a pull request without merging.

**Usage:**
```bash
jj spr close [OPTIONS]
```

**Options:**
- `-r, --revision <REV>` - Revision(s) whose PR to close (default: `@-`)
- `-a, --all` - Close PRs for all changes from base to revision
- `--base <REV>` - Base revision for `--all` mode (default: trunk)

**Examples:**
```bash
# Close PR for parent of working copy
jj spr close

# Close PR for specific change
jj spr close -r <change-id>
```

---

### `jj spr amend`

Update local commit message with content from GitHub PR.

**Usage:**
```bash
jj spr amend [OPTIONS]
```

**Options:**
- `-r, --revision <REV>` - Revision(s) to update (default: `@-`)
- `-a, --all` - Update all changes from base to revision
- `--base <REV>` - Base revision for `--all` mode (default: trunk)

**Use case:** When PR title/description has been updated on GitHub and you want to sync those changes back to your local commit.

---

### `jj spr format`

Reformat the commit messages of one or more changes into jj-spr's sections. Takes the same `-r`, `-a`, and `--base` options as `amend`.

---

### `jj spr stack`

Show the local stack next to its PRs and GitHub stack, and point out anything that does not match. It changes nothing.

**Options:**
- `-r, --revision <REV>` - Top of the stack to show (default: `@-`)
- `-v, --verbose` - Show each PR's checks, reviewers, and unresolved review threads
- `--json` - Print the stack as JSON, bottom first

See [Checking a stack](../user/stack.md#checking-a-stack).

---

### `jj spr sync`

Update the local stack after PRs merged: abandon the merged changes and rebase the rest onto the main branch.

**Options:**
- `-r, --revision <REV>` - Top of the stack to sync (default: `@-`)
- `--dry-run` - Show what would happen without changing anything (the remote is still fetched)
- `[STACK_FILE]` - Sync a stack file's entries and mark the merged ones

See [Syncing after merges](../user/stack.md#syncing-after-merges).

---

### `jj spr patch`

Create local changes from an existing PR, or from every PR in its GitHub stack.

**Usage:**
```bash
jj spr patch [OPTIONS] <PULL_REQUEST>
```

**Options:**
- `--no-stack` - Only fetch this PR, not the rest of its stack
- `--no-checkout` - Create the changes without moving the working copy onto them

See [Working on someone else's stack](../user/stack.md#working-on-someone-elses-stack).

---

### `jj spr cleanup`

List jj-spr branches on GitHub that no open PR uses.

**Options:**
- `--confirm` - Delete those branches (by default it only lists them)

---

### Stack files

`jj spr submit <file>`, `jj spr stack-file init <file> --tips <revset>`, `jj spr stack-file status <file>`, and `jj spr stack-file adopt <file>` work with a stack file, as do `jj spr land <file>`, `jj spr sync <file>`, and `jj spr close <file>`. See [Stack Files](../user/stack-file.md).

---

## Revision Syntax

SPR supports Jujutsu's revision syntax:

- `@` - Current working copy
- `@-` - Parent of working copy
- `<change-id>` - Specific change by ID (e.g., `qpvuntsm`)
- `main@origin` - Remote tracking branch
- `main..@` - Range from main to current
- `a::c` - Inclusive range from a to c

See [Jujutsu revset documentation](https://martinvonz.github.io/jj/latest/revsets/) for more details.
