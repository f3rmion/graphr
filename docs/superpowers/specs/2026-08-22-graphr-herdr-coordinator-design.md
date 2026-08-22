# Graphr–Herdr Coordinator Skill Design

## Context

Graphr and Herdr already provide the two required halves of a multi-agent
development workflow. Graphr gives Codex and Claude compact, immutable code
graphs and complete bounded change reviews over MCP stdio. Herdr gives a
persistent terminal hierarchy, Git worktree creation, named agent processes,
lifecycle state, and commands for prompting and waiting on those agents.

The integration should compose those existing surfaces. Herdr owns terminal
topology and agent lifecycle. Graphr owns repository analysis and review
snapshots. Neither product needs a new protocol, daemon, UI, or shared state
store.

The first release is one coordinator skill. The invoking agent remains the
coordinator in the main checkout, creates Herdr child workspaces only for
independent implementation work, and reviews committed worker branches with
Graphr from the main checkout. This keeps Graphr's explicit-root and immutable
snapshot boundaries intact while avoiding multiple writers in one checkout.

## Goals

- Let one agent coordinate Graphr-assisted implementation and review through
  Herdr.
- Isolate every writing worker in its own Herdr-managed Git worktree.
- Use Graphr to narrow task briefs and review immutable branch commits.
- Reuse Graphr's existing common-Git-directory cache across linked worktrees.
- Reuse a worker for fixes instead of spawning replacements.
- Surface blocked agents and failed checks to the human without guessing.
- Keep the workflow deterministic, compact, and safe to interrupt.
- Add no Rust code, dependency, schema, server mode, or Herdr plugin.

## Non-goals

- A Graphr daemon shared by several MCP clients.
- Sharing snapshot IDs between different worktree identities.
- Automatically indexing every file change or prewarming every repository.
- Automatic task decomposition without coordinator judgment.
- Multiple agents editing the same worktree.
- Automatic merge, rebase, push, pull request creation, branch deletion, or
  worktree removal.
- A declarative workspace-template language, generic orchestrator framework,
  queue, scheduler, database, or recovery journal.
- Herdr sidebar metadata, custom UI, socket protocol changes, or a Herdr plugin.
- Changing Graphr's MCP tools, authorization rules, cache format, or product
  boundaries.

## Chosen Architecture

```text
main Herdr workspace
└── coordinator agent + Graphr MCP
    ├── child worktree workspace → worker agent
    ├── child worktree workspace → worker agent, only when independent
    ├── review role → coordinator by default, separate agent on request
    └── checks tab → ordinary shell process, created on demand
```

The coordinator is the agent in which the skill is invoked. It owns task
selection, worktree creation, worker prompts, result collection, Graphr
review, and the final report. It does not edit a worker's files.

A worker is the only writing agent in one linked worktree. It implements one
independently deliverable task, runs targeted checks while iterating, and
leaves a local commit for review. A second worker is created only when its
files, ordering, and acceptance criteria do not overlap the first worker.

The coordinator performs the first review itself. A separate reviewer is
started only when the user requests independent review or the change is broad,
security-sensitive, or otherwise benefits from a second context. This keeps
the normal path to two agents: coordinator and worker.

## Skill Package and Activation

The implementation adds one file:

```text
.agents/skills/graphr-herdr/SKILL.md
```

The skill has no scripts, assets, generated configuration, or new dependency.
Its description triggers only when the user explicitly asks to coordinate
Graphr-assisted work through Herdr. Merely noticing parallelizable work does
not activate it.

The skill composes the installed Herdr skill and the existing
`graphr-review` skill. It uses the installed `herdr` binary as the authority
for Herdr command syntax and Graphr's exposed MCP tools as the authority for
analysis. If either capability is unavailable, it stops with setup guidance;
it does not reproduce a partial fallback workflow.

## Preconditions

Before creating anything, the coordinator verifies:

1. `HERDR_ENV=1` is present, proving the coordinator is inside a Herdr pane.
2. The current directory is a Git worktree with a named branch.
3. Canonical `git rev-parse --git-dir` and `git rev-parse --git-common-dir`
   results are equal, proving the coordinator is in the repository's primary
   checkout rather than a linked task worktree.
4. The selected parent checkout has no staged, unstaged, or untracked changes.
5. Herdr can identify the caller's current workspace and pane.
6. Graphr `inspect_root` accepts the exact canonical parent worktree.
7. The `graphr-review` skill is available for the later review phase.

A dirty parent checkout is not silently committed, stashed, copied, or
ignored. The coordinator reports that new worktrees would omit those changes
and asks the human to resolve the state. A linked coordinator checkout would
risk nested task topology and is rejected. A detached HEAD, unapproved Graphr
root, missing tool, or ambiguous repository identity is terminal for the
workflow.

## Planning and Task Selection

The coordinator first reads the repository instructions and the user's goal.
It uses ordinary repository search to identify candidate files or symbols.
When call relationships or blast radius would change the task boundary, it
builds one explicit Graphr commit snapshot for the parent checkout and uses
`search` or `view` on that snapshot. It does not index merely to warm the
cache.

The default is one worker. The coordinator may create two or more workers only
when each task is independently committable and none depends on another
worker's uncommitted result. Shared schemas, generated files, central
registries, migrations, or ordered refactors keep the work in one worker.

Every worker receives a compact brief containing:

- the goal and explicit non-goals;
- its worktree and branch;
- relevant files, symbols, and graph evidence;
- required invariants and trust boundaries;
- acceptance criteria and targeted checks;
- the required result format: commit, summary, checks, and blockers.

The brief includes evidence, not a copied Graphr transcript. Snapshot IDs and
node references remain with the exact Graphr server and worktree identity that
produced them.

## Worktree and Agent Creation

The coordinator uses `herdr worktree create`, not a separate `git worktree
add`, so Herdr records and groups the child workspace. An explicit user or
repository branch convention wins; otherwise the branch name uses the
deterministic `agent/<task-slug>` form. Existing branches or paths are never
overwritten; a collision is reported for human choice.

All Herdr creation calls preserve the user's focus with `--no-focus`. The
coordinator reads the returned workspace, tab, and root-pane IDs from JSON and
never predicts them from sidebar order.

The worker kind follows an explicit user request. Otherwise it matches the
coordinator's detected agent kind when Herdr supports it, falling back to
Codex when its executable is available. If no eligible agent is installed,
the workflow stops before startup. No model name or reasoning level is
hard-coded. The worker alias is a unique, normalized form of
`worker-<task-slug>`.

The coordinator starts the worker in the new workspace's root pane and sends
the task brief through `herdr agent prompt`. It does not type raw keys into the
pane or rely on the UI-focused terminal.

## Coordination and Blocked State

Herdr lifecycle waits replace terminal polling. A submitted worker turn is
waited on in bounded intervals so the coordinator can keep the human informed.
After a timeout, the coordinator inspects the agent state and continues waiting
only while the same agent remains working.

When a worker becomes blocked, the coordinator reads the relevant output and
surfaces the exact question or approval request to the human. It does not grant
permissions, answer product questions, or infer missing authority on the
worker's behalf.

When a worker reports completion, the coordinator verifies the worktree and
commit itself. A worker report is not evidence. The expected handoff is:

- a local branch and commit ID;
- a clean worker worktree;
- a short behavior summary;
- targeted checks with their exit status;
- explicit unresolved blockers or skipped coverage.

If terminal scrollback truncates the handoff, the coordinator asks the same
worker to write it under a temporary directory and return only that path. It
does not add transient coordination files to the repository.

## Graphr Review

The coordinator reviews each committed worker branch from the authorized main
checkout. Graphr's commit target can name an unchecked-out branch or commit,
so the review does not need the worker's linked root or a broader
`--allow-root` list.

The coordinator invokes `graphr-review` with the main worktree as the explicit
root, the captured parent head as base, the worker commit as head, and a commit
target. It follows that skill's complete cursor, remediation,
provenance, and completeness rules.

Findings return to the same worker. The worker amends through a new commit or
commit series according to repository policy, reports the new head, and the
coordinator builds a fresh immutable review snapshot. No old snapshot is
treated as proof of the updated branch.

A separate review agent, when justified, runs in a new Herdr pane or tab in
the main workspace and receives the branch name plus the exact review request.
It remains read-only. Its report is verified by the coordinator before use.

## Checks and Completion

Workers run the smallest targeted tests that guide implementation. The full
repository gate runs once after the final reviewed branch state, in an
ordinary Herdr checks pane rather than another coding agent. For Graphr that
gate is:

```text
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
cargo build --locked --release
```

The commands run serially and stop on the first failure. A failure returns to
the same worker with the relevant output; successful earlier commands are not
presented as proof that the complete gate passed. After repair, the complete
gate restarts from the first command.

The first release does not merge worker branches. The coordinator reports the
reviewed branch and commit, checks run, review completeness, live Herdr
workspace/agent identifiers, and any remaining integration step. It leaves
workspaces, branches, and worktrees intact. Cleanup occurs only after an
explicit human request.

## Failure Handling

- **Not inside Herdr:** stop and explain that orchestration requires a
  Herdr-managed pane.
- **Graphr unavailable or root rejected:** stop; never substitute a live diff,
  another checkout, or an older snapshot.
- **Dirty or detached parent:** stop before worktree creation.
- **Worktree or branch collision:** report the exact collision and request a
  new name or explicit reuse decision.
- **Agent startup failure:** retain the created worktree and report its IDs;
  do not delete it automatically.
- **Blocked agent:** surface the question to the human.
- **Unknown agent state:** inspect output but do not call it complete.
- **Worker verification mismatch:** report the actual Git state and resume the
  same worker when safe.
- **Incomplete Graphr coverage:** report review as incomplete and do not claim
  approval.
- **Check failure:** preserve the failing output and return it to the worker.

## Efficiency Constraints

- Do not prewarm Graphr until measured indexing latency justifies it.
- Do not launch a worker before confirming independent work exists.
- Do not launch a separate reviewer on the normal path.
- Do not give every worker Graphr MCP access in the first release; the
  coordinator supplies bounded graph evidence and performs commit review.
- Do not widen Graphr authorization for convenience.
- Do not repeat full repository checks after every small fix.
- Do not replace a worker when the same agent can apply review feedback.
- Do not poll pane text when Herdr exposes a lifecycle wait.

These constraints deliberately optimize for fewer agents, fewer model tokens,
and fewer full builds while retaining worktree isolation and complete final
review evidence.

## Verification and Acceptance

Implementation is accepted when the skill can be exercised in an isolated
Herdr test session against a temporary Git repository and demonstrates:

1. explicit activation and a clean stop outside Herdr;
2. refusal of dirty, detached, or Graphr-unauthorized parent roots;
3. creation of one grouped child worktree without changing focus;
4. launch and stable naming of one worker agent;
5. a task brief containing boundaries, evidence, checks, and handoff format;
6. correct handling of working, blocked, done, unknown, and failed states;
7. independent verification of the worker's branch, commit, cleanliness, and
   checks;
8. a complete Graphr commit review from the parent root without authorizing the
   child root;
9. feedback sent to the original worker and a fresh review of the new head;
10. no merge, push, cleanup, server stop, or mutation of user-owned Herdr
    resources without an explicit request.

The repository diff for the first implementation contains only the new
`SKILL.md`. Scenario verification uses temporary repositories and isolated
Herdr sessions rather than committed fixtures. Graphr's Rust sources, Cargo
metadata, MCP behavior, and cache format remain unchanged.

## Deferred Extensions

A shell helper or Herdr plugin may be considered only after repeated use shows
that skill-driven worktree and agent creation is the dominant source of
friction. Per-worker Graphr MCP authorization may be added only when workers
repeatedly need dirty-worktree graph queries that cannot be represented in the
coordinator's task brief. Sidebar risk metadata, automatic indexing, persistent
queues, and merge automation remain separate designs.
