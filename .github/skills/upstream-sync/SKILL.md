---
name: upstream-sync
description: Use when rebasing or synchronizing this Neomacs fork with upstream, especially when conflicts, release workflows, fork-only behavior, or a force-push are involved.
---

# Upstream Sync

## Overview

An upstream sync is a semantic port of the fork's intent onto the new
upstream implementation. A conflict-free rebase is not proof that fork
behavior survived.

**Core principle:** inventory fork invariants before rebasing, resolve against
upstream's new design, verify those invariants, then push once with an exact
lease.

Work in the current checkout. Do not create a worktree.

## Non-negotiables

- Never use whole-file `ours` or `theirs` as a conflict strategy.
- Never use `git clean`, `git reset --hard`, or checkout-based file reverts.
- Never create backup branches or tags unless the user explicitly requests
  one. Recorded SHAs and the reflog are the default recovery path.
- Never weaken a validator to make a changed package pass.
- Never create committed sync reports, ADRs, or `docs/upstream-sync` files.
- Keep the sync ledger in session state and summarize it in the final handoff.
- Do not push until the fork commit stack and affected invariants are checked.
- If the user already requested the rebase and force-push, do not ask again
  before routine conflict resolution, testing, or the lease-protected push.

## Phase 1: Capture the Before State

Fetch both remotes, then record:

```powershell
git fetch origin
git fetch upstream
git rev-parse HEAD
git rev-parse origin/main
git rev-parse upstream/main
git merge-base HEAD upstream/main
git log --reverse --oneline upstream/main..HEAD
git status --short
```

The ledger must contain:

- current branch;
- local, `origin/main`, `upstream/main`, and merge-base SHAs;
- ordered fork-only commits;
- changed paths for each fork-only commit;
- known fork invariants affected by those paths;
- pre-existing tracked and untracked work.

Do not move, delete, stash, or commit unrelated user files. Generated SDK
directories may remain untracked.

## Phase 2: Inventory Fork Intent

Read every fork-only commit before rebasing:

```powershell
git show --stat --oneline <sha>
git show --format=fuller --find-renames <sha> -- <affected-paths>
```

For each commit, write one ledger entry:

| Field | Question |
|---|---|
| Intent | What user-visible or maintenance behavior does this commit preserve? |
| Surface | Which files, entry points, and external contracts implement it? |
| Upstream movement | Did upstream replace, rename, or centralize that implementation? |
| Verification | What focused command proves the intent still holds? |

Do not treat the old patch text as the invariant. The invariant is the
behavior the patch was meant to provide.

## Release Contract

When `.github/workflows/release.yml` or Windows packaging changes, preserve
all of these invariants:

1. `prepare-release` creates the GitHub release before builds finish.
2. Linux x86_64 publishes one `.deb`.
3. Linux aarch64 publishes one `.tar.gz`.
4. Windows x86_64 and aarch64 each publish one `.zip`.
5. Each build uploads directly to the existing release and can succeed
   independently of other platforms.
6. There are no macOS, Docker, aggregate artifact, installer, or published
   install-verification jobs.
7. Workflow-dispatch releases use the tag and version produced by
   `prepare-release`, not `GITHUB_REF_NAME`.
8. Windows GStreamer packaging points `GSTREAMER_ROOT` at the extracted
   runtime-only MSI tree while invoking
   `vendor-windows-gstreamer-runtime.sh`, then restores the prior value.
9. The ZIP validator continues rejecting SDK-only `.pdb`, `.h`, `.a`, and
   `.lib` files.

Upstream action-version bumps, runner updates, cache fixes, and build fixes may
be adopted only when they do not violate this contract.

## Phase 3: Rebase Semantically

Rebase the current branch directly:

```powershell
git rebase upstream/main
```

For every conflict:

1. Identify which fork commit is being replayed.
2. Read the full conflicted function, job, or script boundary.
3. Inspect the relevant upstream commit and current callers.
4. State the upstream intent and fork intent separately in the ledger.
5. Implement the smallest result that preserves both intents.
6. Stage only the resolved paths and continue the rebase.

Prefer upstream's new abstraction and port the fork behavior into it. Do not
restore code that upstream replaced with a centralized helper.

Review high-risk files even when Git reports no conflict:

- `.github/workflows/release.yml`;
- packaging scripts and their environment-variable interfaces;
- daemon/process client construction;
- persistent-cache paths and enablement conditions.

Auto-merge is especially unsafe when upstream renamed an environment variable
or moved responsibility between caller and callee.

## Phase 4: Compare the Commit Stacks

After the rebase, compare the old and new fork ranges:

```powershell
git range-diff <old-merge-base>..<old-head> upstream/main..HEAD
git log --reverse --oneline upstream/main..HEAD
git diff --check
git status --short
```

For every original fork commit, account for its rebased equivalent. A changed
patch is acceptable only when the ledger explains how upstream movement
required it.

Unexpected missing commits, absorbed behavior, or unrelated file changes stop
the sync. Investigate before testing or pushing.

## Phase 5: Focused Verification

Run the smallest existing checks that cover every changed fork surface. Run
independent checks in parallel.

For release and Windows packaging changes:

```powershell
python scripts\test-release-workflow.py
pwsh -NoProfile -File scripts\test-windows-gstreamer-setup.ps1
```

For daemon/process or native-cache changes, run their focused test selectors.
Do not run the full `neovm-core` suite unless a focused failure shows that
broader coverage is required.

Use a five-minute timeout for tests without compilation and a fifteen-minute
timeout when compilation is included. Do not create a throwaway release tag
or consume release CI merely to validate YAML unless the user explicitly asks
for an end-to-end release run.

Record each command, result, and any platform behavior that could not be
tested locally.

## Phase 6: Push Once

Immediately before pushing:

1. Re-read the recorded pre-sync `origin/main` SHA.
2. Confirm the remote has not moved.
3. Confirm the range-diff is fully accounted for.
4. Confirm all focused checks are green.
5. Confirm the worktree contains no unexpected tracked changes.

Push with an explicit lease:

```powershell
git push --force-with-lease=main:<recorded-origin-main-sha> origin main
```

If the lease fails, stop. Fetch and reassess; never replace it with
`--force`.

## Final Handoff

Report:

- before and after local, origin, and upstream SHAs;
- rebased fork-only commits;
- conflict and silent-semantic decisions;
- focused verification results;
- exact push result;
- remaining unvalidated platform-specific behavior, if any.

Do not add a committed report. The handoff and transient session ledger are
the record.

## Rationalizations to Reject

| Rationalization | Required response |
|---|---|
| "A worktree protects the dirty checkout." | Worktrees are forbidden here; preserve unrelated files in place. |
| "The YAML parses, so the release merge is correct." | Check the explicit release contract; valid YAML can publish the wrong assets or publish too late. |
| "Take upstream's file and restore fork behavior later." | Resolve the replayed fork intent during the rebase unless the user requested a separate change. |
| "Upstream added useful macOS, Docker, or install checks." | The fork's explicit release contract excludes them. |
| "Filter `.pdb` files and keep packaging." | Wrong-root packaging is the bug; keep the validator and fix the caller/vendor contract. |
| "Create an ADR so the next sync remembers." | Keep the ledger transient; this skill is the durable procedure. |
| "Run every suite and a throwaway release to be safe." | Use focused checks first; expand only from evidence or an explicit request. |
| "A normal force-push is faster." | Use the recorded exact lease or do not push. |
