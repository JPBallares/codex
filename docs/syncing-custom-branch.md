# Keep `custom` Up To Date With `upstream/main`

A minimal, low-conflict workflow assuming:
- `origin` = your fork, `upstream` = original repo
- Your `main` mirrors `upstream/main` (no local changes)

## One-Time Setup

```bash
# Add and verify upstream remote
git remote add upstream <original-repo-url>  # once
git remote -v
```

## Sync `main` (mirror upstream)

```bash
git fetch upstream --prune
git switch main
git reset --hard upstream/main   # keeps main identical to upstream
# optionally mirror to your fork
git push --force-with-lease origin main
```

## Update `custom` (rebase onto upstream)

```bash
git switch custom
# rebase your work on top of upstream/main
git fetch upstream
git rebase upstream/main
# resolve conflicts if any, then
#   git add <files>
#   git rebase --continue
# to abort: git rebase --abort

# update your fork
git push --force-with-lease origin custom
```

Why rebase: avoids “upstream vs upstream” conflicts that happen when merging upstream into a branch with prior merge history. Your commits replay cleanly on top of the latest upstream.

## If You Prefer Merges (alternative)

```bash
git switch custom
git fetch upstream
git merge --no-ff upstream/main   # create a merge commit
# resolve, commit, then push
```

## Quality-of-Life (recommended)

```bash
git config --global pull.rebase true     # rebase by default
git config --global rebase.autoStash true # stash/unstash automatically
git config --global rerere.enabled true   # reuse conflict resolutions
```

## TL;DR

```bash
# mirror main
git fetch upstream && git switch main && git reset --hard upstream/main && git push --force-with-lease origin main

# rebase custom on latest upstream
git switch custom && git fetch upstream && git rebase upstream/main && git push --force-with-lease origin custom
```

