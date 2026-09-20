# Multi-project session scope design

Date: 2026-09-20

## Goal

Keep a single CatDesk workspace rooted at `/home/morts/dev` so one connector can access all repositories, while making ordinary work project-scoped so parallel MCP sessions can work efficiently and independently in different repositories.

The workspace remains the security boundary. A session-local project scope becomes the default execution/context boundary.

## Problem

Today, commands without an explicit `cwd` resolve to the workspace root. With a broad workspace such as `/home/morts/dev`, that makes change discovery and other recursive work potentially span many unrelated repositories. It also means project-specific context such as handoffs and `AGENTS.md` is derived from the workspace rather than the repository the session is actually working on.

Running a separate CatDesk instance per repository would avoid broad scans but would lose the desired single-connector, multi-project workflow.

## Design principles

1. `workspace_root` remains `/home/morts/dev` and remains the path-security boundary.
2. Each MCP session may have one `active_project` inside the workspace.
3. Different MCP sessions may hold different active projects simultaneously.
4. Explicit paths and explicit `cwd` always win over the session default.
5. The workspace root remains a valid explicit scope for cross-project operations.
6. Project scope affects defaults and context selection; it must never widen filesystem access beyond `workspace_root`.

## Project identity

A project is identified by a canonical directory path inside `workspace_root`.

When CatDesk needs to infer a project from a path, it walks upward from that path until it finds the nearest Git worktree/repository boundary. The repository root becomes the project root. If no Git root is found before `workspace_root`, the nearest top-level workspace child containing the path is used as the project root. The workspace root itself is used only when the operation explicitly targets it or no narrower project can be inferred safely.

Canonical paths are used internally so symlinks cannot create distinct identities for the same project.

## Session-local active project

CatDesk stores `active_project: Option<PathBuf>` alongside the existing MCP session state.

The active project is updated only from an explicit, successfully resolved project signal:

- `run_command` / `start_command` with explicit `cwd`;
- file tools with an explicit path that resolves to a project;
- a future explicit project-selection tool, if one is added later.

Read-only control calls such as `catdesk_instruction`, `poll_command`, and `cancel_command` do not implicitly change the active project.

A session cannot set `active_project` outside `workspace_root`.

## Command defaulting

For `run_command` and `start_command`:

1. If `cwd` is provided, resolve and use it exactly as today.
2. Otherwise, if the MCP session has an `active_project`, use that project root as the effective `cwd`.
3. Otherwise, fall back to `workspace_root` for backward compatibility.

The structured tool result continues to report the effective `cwd`.

Explicit `cwd=/home/morts/dev` remains the supported way to request a cross-project command.

Background jobs retain both their effective `cwd` and owning MCP session. Existing job ownership/isolation remains unchanged.

## Change tracking

Change tracking remains scoped to the effective operation target.

For commands, the discovered change scope uses the effective `cwd`, including the session-derived project default. This prevents an ordinary project command from recursively snapshotting all repositories under `/home/morts/dev`.

Explicit workspace-root commands may still track the workspace root because that is an intentional cross-project operation.

## File tools

`read`, `read_image`, `search`, `write`, `edit`, and `delete` keep accepting paths relative to `workspace_root` so a session can deliberately access another project without reconnecting CatDesk.

When an explicit file/directory path clearly belongs to one project, that project becomes the session's active project after successful path resolution. Merely searching the workspace root does not switch the session to workspace-wide mode.

A later tool call can therefore move a session from one project to another by explicitly addressing that project, while other MCP sessions remain unaffected.

## Project-aware AGENTS.md

Instruction resolution becomes layered rather than workspace-only.

Order, from broadest to most specific:

1. configured global CatDesk/Codex instructions according to the existing `AgentsPathMode` behavior;
2. workspace-level `/home/morts/dev/AGENTS.md`, when present;
3. active-project `<project>/AGENTS.md`, when present.

Project instructions are appended as the most specific layer and therefore govern project-specific work without removing global safety/workflow instructions.

`catdesk_instruction` is still callable before an active project exists. In that case it returns the existing global/workspace instruction set. Once a project has been established, subsequent instruction/context generation may include the project layer.

No project `AGENTS.md` may be read from outside `workspace_root`.

## Project-aware handoffs

Handoff identity must no longer be based solely on `workspace_root` when a session has an active project.

Handoff identity is derived from:

- the active project canonical path when present;
- otherwise the workspace canonical path as a backward-compatible fallback.

This gives independent persistent handoffs for projects such as `project-depth`, `poe_pricer`, and `AgentForge` even though they share one CatDesk workspace.

`create_handoff` collects Git branch/status/recent commits from the active project root when one exists. An explicit workspace-level handoff remains possible when the active scope is the workspace root.

The generated search prefix must be stable for the same canonical project and distinct for different project roots.

## Parallelism and isolation

Project state is keyed by MCP session namespace, not globally.

Example:

- session A: active project `/home/morts/dev/project-depth`;
- session B: active project `/home/morts/dev/poe_pricer`;
- session C: active project `/home/morts/dev/AgentForge`.

All three may run jobs concurrently subject to the existing global process/admission budgets. Switching session A to another project must not alter B or C.

## Compatibility

Clients that do not provide a session namespace keep current behavior: missing `cwd` resolves to `workspace_root` and handoffs use workspace identity.

Existing explicit `cwd` semantics, path-escape protection, process budgets, command-job ownership, and MCP response shapes remain compatible.

No new public MCP tool is required for the first implementation. Project selection is inferred from explicit paths/cwd, minimizing API surface.

## Failure handling

- A project candidate outside `workspace_root` is rejected using the existing path-escape behavior.
- A missing/deleted active project is cleared and the operation falls back to normal resolution rather than retaining a stale path.
- Failure to discover a Git root is not an error; CatDesk uses the top-level workspace child fallback.
- Project detection must not recursively scan the entire workspace.

## Observability

Diagnostics should record project selection/switch events without leaking arbitrary command contents:

- `session_project_selected`;
- `session_project_changed`;
- `session_project_cleared`.

Where practical, command diagnostics should expose whether effective cwd came from `explicit`, `session_project`, or `workspace_fallback`.

## Test strategy

Implementation follows TDD. Required behavior tests include:

1. two MCP sessions can hold different active projects simultaneously;
2. explicit command `cwd` selects that project for the session;
3. a subsequent command without `cwd` uses the session project;
4. another session without a project still falls back to workspace root;
5. explicit workspace-root `cwd` overrides project default;
6. session project cannot escape workspace root;
7. switching one session does not affect another;
8. background jobs retain the project-derived cwd and session ownership;
9. command change tracking is rooted at the effective project cwd;
10. project-aware handoff prefixes differ between repositories and are stable within one repository;
11. handoff Git context comes from the active project;
12. project `AGENTS.md` is included only for the active project and layered after broader instructions;
13. non-session/legacy request paths preserve current behavior;
14. stale/deleted active projects safely fall back/clear;
15. project inference does not recurse across sibling repositories.

## Non-goals

- Running one CatDesk process per repository.
- Removing the broad workspace security root.
- Automatic distributed scheduling across machines.
- Per-project process quotas in this change.
- A project picker UI or new MCP project-selection tool in the first version.

## Expected outcome

A single CatDesk instance can safely serve many repositories under `/home/morts/dev`. Each chat/session naturally stays on its own repository for commands, change tracking, instructions, and handoffs, while deliberate cross-project access remains available through explicit paths or workspace-root `cwd`.