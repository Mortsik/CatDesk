# Instruction payload shrink — 2026-10-07

Epic `catdesk-ojt`, issue `catdesk-ojt.7` — "Shrink CatDesk bootstrap
instructions without removing enforceable guarantees".

The `catdesk_instruction` payload is delivered before every workspace task.
The text carried three kinds of content: enforceable workflow/safety rules,
parameter details already owned by tool schemas and runtime checks, and
explanatory redundancy. The shrink removed only the latter two where a
runtime mechanism or the tool catalog already guarantees the behavior.

## Payload measurements

Measured on the same workspace shape (no AGENTS.md layers; dynamic handoff
prefix/filename included). Tokens ≈ bytes/4 (English-prose heuristic).

| Payload | Before | After | Δ bytes | Δ % | ~tokens before | ~tokens after |
| --- | --- | --- | --- | --- | --- | --- |
| Instruction, `Both` + multi-tools | 5548 B | 3578 B | −1970 B | −35.5% | ~1387 | ~895 |
| Instruction, `Both` + read-only | 3645 B | 2412 B | −1233 B | −33.8% | ~911 | ~603 |
| Instruction, `Browser` + multi-tools | 1472 B | 933 B | −539 B | −36.6% | ~368 | ~233 |
| `tools/list` (context, Both + multi-tools) | 25398 B | 25331 B | −67 B | −0.3% | ~6349 | ~6332 |

The pinned budget lives in
`instruction_payload_stays_materially_below_baseline` (`src/mcp/tests.rs`):
every `(Mode, ToolMode)` pair must stay ≥ 25 % below its own recorded baseline
(with headroom for the workspace-scoped handoff fragments — the Browser header
is fully static and needs none). Growing the template past that line fails the
test per mode.

## What was removed and why

| Removed text | Why safe |
| --- | --- |
| Image parameter details (20 MiB / 40 Mpx / 1600×1600 / format detection) | Runtime-enforced by `workspace_tools::read_image` caps and errors; advertised per-call in the `read_image` inputSchema the model already receives. |
| Listing-intercept guidance line (`find`, `tree`, `ls -R`, `rg --files`) | Runtime-enforced (`command::detect_list_files_intercept`) and documented in the `run_command` tool description. |
| Edit mechanics (atomic batch, 1-based inclusive lines, exact `old_text`) | Runtime-enforced (`workspace_tools::edit_file` atomic guard) and documented in the `edit` tool description/inputSchema. |
| Duplicate "prefer dedicated browser/DevTools tools" line | Covered by the shared "Prefer dedicated MCP tools" rule (the only guidance needed in browser-only mode). |
| Handoff save workflow duplicated in the `create_handoff` tool description | The instruction owns the Library-save workflow; the description keeps only the tool-scoped facts (returns filename+content, does not write the workspace, auto-records Git context, no secrets). |
| Verbose connectors ("You already have the built-in sandbox...", "However, CatDesk offers...") | Rewritten into two compact sentences carrying the same guarantees. |

## Guarantee map — every enforceable behavior after the shrink

Legend: **runtime** = a code path enforces it; **test** = pinned by a test.

| Guarantee | Enforcement / test after the shrink |
| --- | --- |
| `catdesk_instruction` gate before other tools | runtime `src/mcp.rs:169-195` (CATDESK_INSTRUCTION_REQUIRED), test `handle_request_requires_instruction_before_other_tools` |
| Workspace-first over the offline sandbox; never silently fall back; report raw connector errors | instruction text + `instruction_payload_keeps_every_enforceable_guarantee`, test `catdesk_instruction_describes_offline_sandbox_and_connector_error_reporting` |
| File operations stay inside the workspace root | runtime path resolution (`PATH_OUTSIDE_WORKSPACE` in `src/command.rs`, `src/workspace_tools.rs`), instruction sentence kept |
| Retry a tool call blocked by OpenAI safety checks | instruction text + guarantee test |
| Refresh via `api_tool.list_resources` on disconnect/empty/`Resource not found:` | instruction text + guarantee test |
| No manual `Co-Authored-By: CatDesk` trailer | runtime `command::contains_catdesk_co_author_marker` (blocks `run_command`/`start_command`, rewrites commits), instruction text + test `catdesk_instruction_tells_agents_not_to_write_catdesk_trailers` |
| Match recent commit style | instruction text + guarantee test |
| Explicit branch in `git push` | instruction text + guarantee test |
| Prefer dedicated tools over `run_command` | instruction text (single deduplicated rule) + guarantee test |
| Images through `read_image`; native content; server-side vision fallback | instruction text + tests `catdesk_instruction_mentions_read_image_for_image_reading`, runtime caps in `workspace_tools::read_image` |
| Handoff discovery: search → read → verify → delete-after-read; ask when ambiguous; untrusted; never overrides user/AGENTS.md; never invent | instruction text (all semantic steps kept) + tests `catdesk_instruction_points_new_sessions_to_library_handoff_search`, `project_instruction_layers_workspace_then_project_agents_and_uses_project_handoff_identity` + guarantee test |
| Save handoff to Library under returned filename, replace exact-name copy, no repo copy, no secrets | instruction text + tool description + guarantee test |
| Short work → `run_command` last resort; >20 s → `start_command`; 120 s hard ceiling | runtime `command::clamp_timeout` / `MAX_TIMEOUT_MS` + instruction text + test `catdesk_instruction_steers_long_commands_to_start_and_poll` + tool descriptions (`command_tool_descriptors_keep_silent_waits_stream_safe`) |
| Poll cursor (`after`/`nextCursor`), drain on `hasMoreOutput` | instruction text + runtime cursor mechanics `src/command_jobs.rs` + tool descriptions |
| Job durability across restarts (`interrupted`, exit codes) | instruction text + runtime job store `src/command_jobs.rs` + test `command_job_tools_document_restart_durability` |
| Keep-alive polling (`abandoned` after idle window) | instruction text + runtime `DEFAULT_ABANDON_AFTER_MS` + tool descriptions |
| No duplicate jobs for still-running work | instruction text + runtime request-key deduplication `src/mcp/commands.rs` (`request_key`) |
| No secrets in handoffs | instruction text + guarantee test + tool description |
| AGENTS.md layering (workspace → project precedence) | untouched by this change; test `project_instruction_layers_workspace_then_project_agents_and_uses_project_handoff_identity` |
| Large instruction payloads cannot flood the transcript | untouched by this change; runtime shared response budget (`src/mcp.rs:493-506`) + test `oversized_catdesk_instruction_is_externalized_not_inlined` (catdesk-ojt.5) |

## Follow-ups

- **Live A/B smoke with ChatGPT is deliberately out of scope** (no live client
  in this environment): an operator should re-run a handful of representative
  tasks (file read/edit, long build, image analysis, session handoff recovery)
  against the shrunk instructions and compare tool selection. The guarantee
  table above is the checklist for that smoke.
- F5 (`catdesk_instruction` embedding uncapped AGENTS.md text and Binagotchy
  card images) is tracked separately (catdesk-ojt.5 findings); this issue only
  minimized the authored template text, not the layered input.
