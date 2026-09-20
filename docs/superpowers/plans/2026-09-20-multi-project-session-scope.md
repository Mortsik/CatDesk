# Multi-project session scope implementation plan

> **Spec:** `docs/superpowers/specs/2026-09-20-multi-project-session-scope-design.md`

## Goal

Keep `/home/morts/dev` as CatDesk's single security workspace while giving each named MCP session an independent active project. Commands without `cwd` use that session project, explicit paths/cwd can switch it, anonymous/stateless requests preserve current workspace-root behavior, and handoffs/instructions use the project context without widening access.

## Global constraints

- Follow TDD for every behavior change: write the named test, run it and observe the expected failure, then implement the minimum production change.
- Do not change the public MCP tool set in this work.
- `workspace_root` remains the only filesystem security boundary.
- Explicit `cwd` and explicit file paths retain current semantics.
- Anonymous/stateless requests must not gain sticky project state.
- Project discovery must walk ancestors only; never recurse through sibling repositories.
- Keep existing command-job ownership, process budgets, admission pools and response shapes compatible.
- Do not push during implementation. Commits are local until branch finishing.

## Interfaces

### Project resolver

Create `src/project_scope.rs` with a focused path-only API:

```rust
use std::path::{Path, PathBuf};

pub(crate) fn infer_project_root(
    workspace_root: &Path,
    candidate: &Path,
) -> Result<PathBuf, String>;

pub(crate) fn valid_active_project(
    workspace_root: &Path,
    active_project: Option<&Path>,
) -> Option<PathBuf>;
```

`infer_project_root` canonicalizes the workspace and the nearest existing candidate/ancestor, rejects escapes, walks upward looking for the nearest `.git` marker, and otherwise returns the top-level workspace child that contains the candidate. An explicit candidate equal to the workspace root returns the workspace root. It never scans siblings.

`valid_active_project` returns a canonical in-workspace directory only while it still exists; otherwise `None`.

### Session state

Generalize the server's per-session instruction map into a per-session state store. Named session state contains:

```rust
#[derive(Clone, Debug)]
struct NamedSessionState {
    instruction_called: bool,
    active_project: Option<PathBuf>,
    last_seen: Instant,
}
```

The store keeps the existing one-hour TTL and 1,024-session cap. Anonymous instruction state stays process-global as today, but anonymous requests never store `active_project`.

Required operations:

```rust
fn instruction_called(&self, session: &ClientSession) -> bool;
fn mark_instruction_called(&self, session: &ClientSession);
fn active_project(&self, session: &ClientSession, workspace_root: &Path) -> Option<PathBuf>;
fn set_active_project(&self, session: &ClientSession, project: PathBuf) -> ProjectStateChange;
fn forget(&self, session: &ClientSession);
fn has_named_sessions(&self) -> bool;
```

`ProjectStateChange` distinguishes selected / changed / unchanged so diagnostics can emit only meaningful events.

### MCP request context

Extend `mcp::handle_request_with_session` and its internal tool dispatcher with:

```rust
active_project: Option<&Path>
```

For `run_command` and `start_command`, resolve the effective cwd through one helper:

```rust
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CwdSource {
    Explicit,
    SessionProject,
    WorkspaceFallback,
}

fn resolve_effective_command_cwd(
    workspace_root: &str,
    cwd_input: Option<&str>,
    active_project: Option<&Path>,
) -> Result<(PathBuf, CwdSource), String>;
```

Order: explicit `cwd` > valid active project > workspace root. The generic command change scope must use the same effective cwd so the before/after snapshot matches command execution.

### Project signal extraction

After a request's explicit paths have been safely resolved, a named session may switch project. Add one non-recursive helper (in `project_scope.rs` or `server.rs`) that identifies a project candidate from tool arguments:

- `run_command`, `start_command`: explicit `cwd` only;
- `write`, `edit`, `delete`, `read_image`: explicit `path`;
- `read`: switch only when all successfully resolved paths infer the same project;
- `search`: switch only for an explicit non-workspace-root `path`;
- `catdesk_instruction`, `poll_command`, `cancel_command`, `create_handoff`: no project switch.

The switch happens only for a named MCP session and only after path resolution succeeds. An explicit workspace-root `cwd` intentionally selects workspace-wide scope. A cross-project batch `read` does not switch the session.

### Handoff context

No new public handoff format is needed. Existing `handoff::create_handoff`, `handoff_filename`, and `handoff_search_prefix` already derive identity and Git context from the path supplied by the caller. In MCP code select:

```rust
let context_root = active_project.unwrap_or_else(|| Path::new(workspace_root));
```

and pass `context_root` to handoff identity/Git collection. Anonymous requests keep `workspace_root`.

### AGENTS layers

Replace the single `preferred_agents_text` lookup used by `catdesk_instruction_text` with a layer builder that accepts `active_project`.

Rules:

- `Disabled`: no AGENTS layers.
- `Workspace`: workspace `AGENTS.md`, then project `AGENTS.md` when distinct.
- `Catdesk`: `~/.catdesk/AGENTS.md`, workspace `AGENTS.md`, then project `AGENTS.md`.
- `Codex`: `~/.codex/AGENTS.md`, workspace `AGENTS.md`, then project `AGENTS.md`.
- `Default`: existing preferred global source if one exists (`~/.catdesk/AGENTS.md`, otherwise `~/.codex/AGENTS.md`), then workspace `AGENTS.md`, then project `AGENTS.md`.

De-duplicate canonical paths so workspace and project layers are never repeated. A project AGENTS file is accepted only inside `workspace_root`.

## Task 1: Add bounded project-root inference

**Files:** `src/project_scope.rs`, `src/main.rs`

### RED

Add unit tests in `src/project_scope.rs` before production implementation:

```rust
#[test]
fn nearest_git_root_wins_without_scanning_siblings() { /* workspace/a/.git + workspace/b/.git */ }

#[test]
fn non_git_path_falls_back_to_top_level_workspace_child() { /* workspace/plain/sub/file */ }

#[test]
fn workspace_root_can_be_an_explicit_project_scope() { /* candidate == root */ }

#[test]
fn project_candidate_cannot_escape_workspace() { /* ../outside */ }

#[test]
fn stale_active_project_is_rejected() { /* delete directory after selection */ }

#[test]
fn nonexistent_leaf_uses_nearest_existing_ancestor() { /* project/new/file.txt */ }
```

Run:

```bash
cargo test project_scope -- --nocapture
```

**Expected RED:** compilation/test failure because the resolver functions do not exist or do not implement the required behavior.

### GREEN

Implement ancestor-only discovery. Use filesystem metadata only on the candidate ancestry and `.git` entries; never call recursive walkers or `git status`.

Register `mod project_scope;` in `src/main.rs`.

Run:

```bash
cargo test project_scope -- --nocapture
```

**Expected GREEN:** all project-scope tests pass.

Then run:

```bash
cargo test project_scope command::tests::resolve_workspace_path_defaults_to_workspace_root_for_missing_or_dot_cwd -- --nocapture
```

**Expected:** resolver tests plus existing workspace-path compatibility test pass.

### Commit

`feat: add bounded project scope discovery`

## Task 2: Store active project per named session and use it for command cwd

**Files:** `src/server.rs`, `src/mcp.rs`, optionally `src/project_scope.rs`

### RED A — session isolation

In `src/server.rs`, add tests before production changes:

```rust
#[test]
fn named_sessions_keep_independent_active_projects() { /* session-a -> repo-a, session-b -> repo-b */ }

#[test]
fn anonymous_session_never_keeps_active_project() { /* set is ignored / returns None */ }

#[test]
fn stale_project_is_cleared_without_affecting_instruction_gate() { /* delete repo dir */ }
```

Run:

```bash
cargo test named_sessions_keep_independent_active_projects anonymous_session_never_keeps_active_project stale_project_is_cleared_without_affecting_instruction_gate -- --nocapture
```

**Expected RED:** missing project state API / assertions fail.

### GREEN A

Generalize `InstructionGate` into a session-state store while preserving TTL/capacity and all instruction-gate behavior. `delete_mcp` forgets the entire named session state as one operation.

Run the three new tests plus existing session-gate tests:

```bash
cargo test session -- --nocapture
```

**Expected:** new project-state tests and existing session isolation/job tests pass.

### RED B — effective command cwd

In `src/mcp.rs`, add focused tests before changing handlers:

```rust
#[test]
fn effective_cwd_prefers_explicit_then_session_project_then_workspace() { /* all three sources */ }

#[tokio::test]
async fn run_command_without_cwd_uses_session_project() { /* command prints pwd */ }

#[tokio::test]
async fn start_command_without_cwd_uses_session_project() { /* job snapshot cwd */ }

#[tokio::test]
async fn legacy_request_without_session_project_still_uses_workspace_root() { /* compatibility */ }

#[tokio::test]
async fn project_default_scopes_command_change_tracking_to_that_project() {
    /* sibling file changes outside project are not reported */
}
```

Run each named test or a narrow `cargo test effective_cwd` / `cargo test session_project` set.

**Expected RED:** missing active-project parameter/helper, or cwd remains workspace root.

### GREEN B

Thread `active_project: Option<&Path>` through `handle_request_with_session` and `handle_tools_call_with_session`. Make `run_command`, `start_command`, and `change_scope_for_request` use the same `resolve_effective_command_cwd` helper. Do not change explicit cwd path validation.

In `server::post_mcp_inner`, fetch the current valid session project before calling MCP. If a stored project is stale, clear it and emit `session_project_cleared`.

Run:

```bash
cargo test effective_cwd -- --nocapture
cargo test session_project -- --nocapture
cargo test command_job -- --nocapture
```

**Expected GREEN:** new cwd tests pass; existing command-job ownership/idempotency tests stay green.

### RED C — explicit signals switch only the owning session

Add HTTP-level tests in `src/server.rs`:

```rust
#[tokio::test]
async fn explicit_command_cwd_selects_project_for_followup_command() { /* same session */ }

#[tokio::test]
async fn switching_session_a_does_not_change_session_b_project() { /* two sessions */ }

#[tokio::test]
async fn explicit_workspace_cwd_overrides_and_selects_workspace_scope() { /* deliberate cross-project */ }
```

Use `pwd`/platform equivalent through real command handling, not a mock.

**Expected RED:** follow-up no-cwd command still uses workspace root.

### GREEN C

After explicit signal resolution, update only the named session's project state. Emit:

- `session_project_selected` for first selection;
- `session_project_changed` for a different project;
- no event for unchanged selection.

Never log the raw session id or arbitrary command text in these events.

Run the new HTTP tests and existing parallel named-session stress test.

### Commit

`feat: scope commands to MCP session project`

## Task 3: Learn project from explicit file operations

**Files:** `src/server.rs`, `src/project_scope.rs`, `src/mcp.rs` only if helper placement requires it

### RED

Add request-signal tests covering all ambiguity rules:

```rust
#[test]
fn file_path_tools_select_the_containing_project() { /* write/edit/delete/read_image */ }

#[test]
fn read_batch_selects_project_only_when_all_paths_agree() { /* same project vs two projects */ }

#[test]
fn search_explicit_subproject_path_selects_project_but_workspace_search_does_not() { }

#[test]
fn control_and_handoff_tools_do_not_change_project() { }
```

Add one HTTP integration test:

```rust
#[tokio::test]
async fn explicit_file_read_can_switch_followup_command_to_another_project() {
    /* session starts repo-a, reads repo-b/file, follow-up run_command without cwd executes in repo-b */
}
```

Run the exact new tests.

**Expected RED:** no project signal extraction exists / follow-up remains old project.

### GREEN

Implement non-recursive project signal extraction. Resolve explicit file paths using the existing workspace-safe path helpers before inference. For `read`, require all resolved paths to map to one identical project. For `search`, `path` equal to workspace root or `.` is deliberately not a switch signal.

Update server state only for named sessions after successful safe resolution. Explicit failed/escaping paths must not mutate the active project.

Run:

```bash
cargo test project_signal -- --nocapture
cargo test explicit_file_read_can_switch_followup_command_to_another_project -- --nocapture
```

**Expected GREEN:** signal tests pass and no path escape changes state.

### Commit

`feat: learn session project from explicit paths`

## Task 4: Make handoffs and AGENTS project-aware, then verify lifecycle

**Files:** `src/mcp.rs`, `src/handoff.rs` only if naming clarity/test access needs a small refactor, `src/server.rs`

### RED A — handoff context

Add MCP/integration tests:

```rust
#[tokio::test]
async fn handoff_uses_active_project_identity_and_git_context() {
    /* workspace contains repo-a/repo-b; active repo-b; prefix + branch come from repo-b */
}

#[tokio::test]
async fn anonymous_handoff_keeps_workspace_identity() { }
```

Reuse real temporary Git repositories. Avoid mocking Git.

**Expected RED:** handoff prefix and Git context still come from workspace root.

### GREEN A

Pass active project context root to `handle_create_handoff`, `handoff_search_prefix`, and `handoff_filename` used in CatDesk instruction text. Keep handoff module's existing stable path hash algorithm.

Run handoff tests plus `cargo test handoff -- --nocapture`.

### RED B — AGENTS layering

Add deterministic tests around an injectable/layer-building helper, not environment-global mutations where avoidable:

```rust
#[test]
fn project_agents_are_appended_after_workspace_layer() { }

#[test]
fn project_agents_from_another_repo_are_not_included() { }

#[test]
fn workspace_and_project_agents_are_deduplicated_when_same_path() { }

#[test]
fn disabled_agents_mode_suppresses_all_layers() { }
```

Where global CatDesk/Codex paths are needed, test the path-selection helper separately rather than mutating the real home directory.

**Expected RED:** current instruction builder returns only one preferred AGENTS file.

### GREEN B

Create a layer builder accepting `(workspace_root, active_project, AgentsPathMode)` and append each readable, canonical, in-scope layer exactly once. Preserve the widget's existing selected-path metadata; this change affects instruction composition, not settings UI semantics.

`catdesk_instruction` before project selection remains valid. When re-called after selection its handoff prefix and AGENTS text reflect the active project.

Run the new AGENTS tests plus existing `catdesk_instruction` tests.

### RED C — stale/forget lifecycle and observability

Add/extend server tests:

```rust
#[tokio::test]
async fn deleting_mcp_session_forgets_active_project_with_instruction_state() { }

#[tokio::test]
async fn deleted_project_falls_back_to_workspace_on_next_command() { }
```

**Expected RED:** stale project persists or delete only forgets instruction state.

### GREEN C

Clear stale project state and emit `session_project_cleared`; ensure `DELETE /mcp` removes both instruction and project state. Keep diagnostics event-only (no paths/session IDs).

Run all session/project/handoff/AGENTS tests.

### Commit

`feat: apply project context to handoffs and instructions`

## Task 5: Full regression and parallel multi-project stress

**Files:** tests only unless a failing regression exposes a production defect

### RED

Add one end-to-end stress test in `src/server.rs` with at least three named sessions and three temporary sibling repos. Each session first selects its repo explicitly, then concurrently issues a mix of:

- `run_command` without cwd;
- `start_command` without cwd + poll;
- `read` inside its own project;
- `create_handoff`.

Assert every returned effective cwd/handoff prefix/project file belongs to the owning session and no job is visible cross-session.

Run that test before any production fix.

**Expected RED:** if Tasks 1–4 are complete, this test may already pass. If it passes immediately, record that it is integration coverage of already-test-driven behavior and do not invent production changes merely to force RED. The individual behaviors already had mandatory RED tests.

### GREEN / regression suite

Run formatting and static checks:

```bash
cargo fmt -- --check
cargo clippy --all-targets --all-features -- -D warnings
```

Run full suite:

```bash
cargo test
```

Run the existing responsiveness/stress tests explicitly if they are filtered or platform-gated in the suite:

```bash
cargo test parallel_named_sessions_with_reused_rpc_ids_keep_workloads_isolated -- --nocapture
cargo test runtime_responsiveness -- --nocapture
```

If a failure is found, use systematic debugging and add a failing regression test before any fix.

### Commit

If test-only coverage was added: `test: stress multi-project session isolation`

## Review focus

Final review must deliberately inspect these failure classes beyond ordinary test pass/fail:

1. **Security:** symlink/nonexistent-path tricks must not let project identity or effective cwd escape `workspace_root`.
2. **Session leaks:** no active project may be shared globally or reused across different `Mcp-Session-Id` values.
3. **Anonymous compatibility:** stateless clients keep workspace-root defaults and no sticky project state.
4. **Stale state:** deleted/moved projects cannot cause permanent failures or unsafe lexical fallback.
5. **Ambiguous file operations:** multi-project batch reads/searches must not silently switch to an arbitrary project.
6. **Change-tracking scope:** before/after snapshots must use the same effective cwd as command execution.
7. **Instruction semantics:** project `AGENTS.md` must be in-workspace, ordered last, de-duplicated, and disabled mode must remain disabled.
8. **Handoff identity:** two projects with the same basename remain distinct via canonical-path hash; Git context comes from the selected repo.
9. **Performance:** project inference is ancestry-only and does not recurse through `/home/morts/dev`.
10. **Concurrency:** process/admission budgets and background-job ownership remain unchanged.

## Completion criteria

- Every behavior in the spec's 15-item test strategy is covered by a passing test.
- Each production behavior was preceded by an observed failing test.
- `cargo fmt -- --check`, strict clippy, and full `cargo test` pass fresh.
- Whole-branch review has no Critical/Important unresolved findings.
- Git status in the feature worktree is clean and all implementation commits are present on the feature branch.
