# Semaphore

Native macOS Signal client: Rust core on [presage](https://github.com/whisperfish/presage) +
SwiftUI/AppKit app. Unofficial, personal use. AGPL-3.0 (inherited from presage).

- Spec: `docs/superpowers/specs/2026-10-15-m1-daily-driver-design.md`
- Plans: `docs/superpowers/plans/`
- Board: <https://github.com/users/pitis/projects/2> (issues in `pitis/semaphore`)

## Working principles

1. **Don't assume. Don't hide confusion. Surface tradeoffs.**
2. **Minimum code that solves the problem. Nothing speculative.**
3. **Touch only what you must. Clean up only your own mess.**
4. **Define success criteria. Loop until verified.**

## Ticket flow

An issue linked or named in chat **is the instruction to run this whole flow, end to end,
without asking whether to start.** Stop and ask only for product decisions the issue does not
answer, never for mechanics.

1. Read the issue fully, and its task in the plan if it points to one. Check for an existing PR
   and branch (`gh pr list --state all`, `git branch -a`).
2. Work in **a worktree**, never by switching branches in the main checkout (other sessions
   share it). New work: `make worktree name=<slug> branch=<type>/<slug>` (`feat/`, `fix/`,
   `chore/`, `perf/`; named right from the start — a PR's branch cannot be renamed); add
   `base=origin/<branch>` to stack on an open PR. An existing branch: its worktree, then
   **`make update` before writing code**. `make card issue=<n> status="In progress"`.
3. Implement test-first. A test proves a fix only if it failed before the fix.
4. Verify: `make format && make check`. UI changes: run the app (`make run`) and look.
5. Keep the spec/plan current on this branch when the change contradicts them.
6. Push and open the PR with `gh pr create`. Title: the effect in a plain sentence. Body: what
   changed, why, how it was tested, `Closes #<n>`. `make card issue=<n> status="In review"`.
7. **Right after opening the PR, without being asked,** run `/pr-review-toolkit:review-pr` on
   the PR's own diff (`git diff origin/main...HEAD`; a stacked PR's base is its parent branch).
   There are no other reviewers, so this is the review. Fix every Critical and Important
   finding on the same branch without waiting, re-run the checks, push, and post one PR comment
   listing each finding and its fix; declining a finding is a stated ruling, not silence. A
   finding that needs a product decision stays unfixed and goes to the owner. Then report the
   same in chat. The PR is ready for the owner only after this step.
8. **Merge only on the owner's go**: squash (`gh pr merge --squash --delete-branch`), then
   `make card issue=<n> status="Done"`.

## Worktrees

- `make worktree` creates `worktrees/<PR number>--<slug>` (the number is a guess until the PR
  exists): a new branch starts from the latest `origin/main` (or `base=origin/<branch>`); an
  existing branch is checked out as is. Build output lives per worktree (`core/target`,
  `build/`), so the first build in a new worktree is slow.
- `make update` merges `origin/main`, or the stacked base until its PR is merged, into the
  branch; on a conflict it makes no merge and prints the `git merge` to run.
- Remove a worktree with `git worktree remove` once its PR is merged or closed (check
  `git status` there first).

## Git

- **Never `--no-verify`.** The hooks in `.githooks/` (enabled by `make setup`) and `make check`
  are the only gate; there is no CI. Fix the cause or stop and ask.
- **Never force-push, never rebase a pushed branch.** To catch up: `make update`. Same for
  `commit --amend` or `reset --hard` on pushed commits.
- One issue = one PR; review fixes go on the same branch. No `#NNN` in commit messages (only in
  PR bodies).
- No AI attribution anywhere (no `Co-Authored-By`, no "Generated with").
- Screenshots and test evidence stay out of the repo.

## Stack rules

- presage is pinned to one git rev in `core/sig-core/Cargo.toml`; bump it only in its own PR.
  Keep the `[patch.crates-io] libsqlite3-sys` entry in `core/Cargo.toml` in sync with presage's.
- No `cdsi` feature (BoringSSL/OpenSSL conflict with SQLCipher).
- presage's `Manager`/store are `!Send`: call them only from one dedicated thread (the core's actor, Task 5).
- Never log message content; ids and errors only.
- Swift bindings and the xcframework are generated (`make core`); never edit `app/Generated/`.
- The Xcode project is generated from `app/project.yml` (`make project`); never commit it.
- arm64 only, macOS 15+. No "Signal" name or logo in the app.
- The app is signed with the owner's Apple Development certificate (team `95KQ82C586`,
  `app/project.yml`): the Keychain trusts rebuilds only under a stable signature. Builds need
  that certificate in the login keychain.

## Commands

`make setup` · `make core` · `make project` · `make app` · `make run` · `make format` · `make lint` ·
`make test` · `make check` ·
`make worktree name=… branch=…` · `make update` · `make card issue=… status=…`
