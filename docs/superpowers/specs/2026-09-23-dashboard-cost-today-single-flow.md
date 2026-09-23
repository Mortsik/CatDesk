# Dashboard Cost Today + Single Flow Spec

## Goal

Make the Status dashboard show useful persistent daily/all-time cost telemetry while rendering exactly one live CatDesk↔ChatGPT flow row.

## Requirements

- Add one `COST TODAY` row to Status.
- `COST TODAY` must survive CatDesk restarts by using persisted per-calendar-day usage, not session-only data.
- Show today's spend plus a meaningful average derived from persisted usage; do not invent elapsed-time data that is not tracked.
- Extend `COST TOTAL` with number of tracked usage days and average cost per tracked usage day.
- Daily usage must retain per-model buckets so cost calculation remains correct if pricing differs by model later.
- Existing installs have no historical per-day breakdown. Do not rewrite historical all-time tokens into today's bucket. The tracked-day average begins with data collected after this feature exists.
- Resetting token billing must also reset daily billing history.
- In normal Status mode render exactly one `Your computer ─…─ ChatGPT Web …` row total, selected from the most recently active visible flow.
- Preserve the existing animated flow lane, current tool/action label, and latest-turn token/cost metadata on that row.
- Bootstrap/connect-guide views keep their dedicated behavior.
- Keep English and Traditional Chinese dashboard labels coherent.
- Add regression tests for single-row rendering, daily persistence/accounting, reset-compatible persistence, and dashboard text.
