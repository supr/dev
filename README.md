# dev

`dev` is a lightweight, task-oriented development control plane. A task gets one workspace,
one Git worktree per repository, and one tmux session containing Codex plus a regular shell.

The implementation intentionally keeps task state on disk and leaves final integration under
human control. It does not include a database, dashboard, scheduler, message bus, or automatic
merge behavior.

## Install

Requirements: Rust, Git, tmux, and Codex CLI 0.128.0 or newer (for Goals).

```sh
cargo install --path .
```

## Configure a project

Create `project.toml` at the product workspace root. Repository sources can be remote URLs or
local paths accepted by `git clone`.

```toml
[project]
name = "myproject"

[repositories]
frontend = "git@git.example.com:org/frontend.git"
backend = "git@git.example.com:org/backend.git"
shared = "git@git.example.com:org/shared.git"
```

`dev` discovers this file by walking upward from the current directory. Main clones are stored
under `main/<repository>`, while task worktrees are stored under `tasks/<task>/<repository>`.

## Use

```sh
dev task new oauth
dev task new oauth --objective "Implement Google and GitHub login" \
  --repositories frontend,backend,shared
dev task start oauth
dev task start oauth --plan
dev task start oauth --multi-agent
dev task start oauth --new-session
dev task attach oauth
dev task status oauth
dev task list
dev task stop oauth
dev task destroy oauth
```

`task new` prompts for an objective when `--objective` is omitted. It generates:

- `.dev-task.toml`: lifecycle metadata
- `.dev-status`: machine-readable semantic status (`created`, `working`, `complete`, or `blocked`)
- `OBJECTIVE.md`: the objective and evidence-based completion contract
- `AGENTS.md`: stable multi-repository operating instructions
- one worktree for every selected repository on branch `dev/<task>`

Creation rollback is accountable. If a later repository fails, `dev` removes every worktree it
successfully registered. When rollback succeeds, it removes the partial task directory; when any
cleanup step fails, the command reports both the creation and cleanup errors and preserves the
directory for recovery instead of hiding potentially stale Git worktree state.

`task start` starts a detached tmux session named `dev-<project>-<task>`. The first pane launches
Codex from the task root with instructions to create a Goal from `OBJECTIVE.md`; the second pane
is an ordinary shell for tests, logs, and review. Normal implementation sessions run with a
`workspace-write` sandbox and an approval policy of `never`, so operations inside the task
workspace do not pause for approval; attempts outside that boundary fail and must be reported as
blockers. Use `--attach` to attach immediately.

`task attach` uses `tmux attach-session` from a normal shell. When invoked from inside an existing
tmux client, it uses `tmux switch-client` instead, avoiding a nested-session failure.

`task list` and `task status` distinguish semantic task state from tmux state. The generated task
contract makes `.dev-status` coordinator-owned: it stays `working` during plans, approval waits,
implementation, and verification; becomes `complete` only after the Goal's evidence is verified;
and becomes `blocked` only when human input is required. Subagents never write it. Consequently a
completed task can show `complete` while its still-open Codex tmux session remains `detached` or
`attached`. On their next start, tasks created before this protocol receive a marked lifecycle
section appended to their existing task-level `AGENTS.md` and a valid `.dev-status`; existing task
instructions are preserved and repeated starts do not duplicate the managed section.

Worker health is tracked separately in `.dev-worker-status`. The Codex pane records `running`
before launch and its exit code afterward. `task list` exposes this as `WORKER`: `running`,
`exited (0)`, `failed (N)`, `stopped`, or `-`. This prevents the diagnostic fallback shell from
being mistaken for a live Codex worker merely because tmux remains available.

The first start creates a Codex conversation. If the task has previously started but its tmux
session no longer exists (for example, after a reboot or `task stop`), `task start` prefers
`codex resume --last`. Codex scopes that lookup to the task workspace, preserving the conversation
without a separate session registry in `dev`. No follow-up prompt is injected while resuming, so a
pending plan is not accidentally approved. Pass `--new-session` to deliberately start over or to
recover when the saved Codex session was removed. Restart-time flags still select the permission
profile, so repeat `--plan` when its automatic-review profile is required.

This behavior uses the stable resume command described in the
[official Codex CLI reference](https://learn.chatgpt.com/docs/cli/reference).

For a complicated task, `--plan` starts Codex's native Plan mode so it inspects the repositories
and waits with an implementation plan before modifying files or activating the Goal. Plan sessions
use `--approve-for-me`, which supplies the workspace-write automatic-review profile: routine reads
and inspection commands run directly while eligible approval requests go through automatic review.
After reviewing the plan, toggle Plan mode off with `/plan`, then approve implementation and
activate the Goal. `/permissions` can change the active permission profile in the same session when
necessary.

After the single-agent workflow is proven for a task, `--multi-agent` asks Codex to use its
built-in subagents under a coordinator model. The main thread retains the Goal, cross-repository
decisions, integration, and final verification. The startup contract begins with bounded,
independent, read-heavy delegation; requires explicit scopes and expected outputs; waits for every
subagent before synthesis; and prohibits concurrent edits to overlapping files. Delegated
implementation must have non-overlapping file ownership and is reviewed and tested by the
coordinator. The flag also composes with `--plan`.

This is prompt-level opt-in rather than a custom scheduler: current Codex releases enable
subagent workflows by default, and subagents inherit the parent session's sandbox and approval
policy. See the [official Codex subagents documentation](https://learn.chatgpt.com/docs/agent-configuration/subagents).

If Codex fails to start or later exits, its pane remains available as a diagnostic shell and prints
the Codex exit status instead of disappearing from the tmux layout.

Within each repository, Codex reads repository-level `AGENTS.md` instructions. When a repository
does not contain `AGENTS.md`, it reads that repository's `CLAUDE.md` if present. This fallback does
not replace the generated task-level `AGENTS.md`.

`task destroy` refuses to remove dirty worktrees unless `--force` is supplied, asks for
confirmation otherwise, and deliberately preserves each task branch.

`task stop` is a no-op for a task that has never started, preserving its `created` state so its
first later start creates a new Codex conversation rather than attempting recovery.

`task list` and `task status` report semantic task status, Codex worker health, and whether the tmux
session is attached or detached. When attached, the number of connected clients is shown.
If one task has corrupt metadata or status, `task list` prints an `error` row for it, continues
showing healthy tasks, and exits nonzero with the combined diagnostics.

## Development

```sh
cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test
```
