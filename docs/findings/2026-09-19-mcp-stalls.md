# CatDesk stalls: investigation on 2026-09-19

## Observed evidence

- The retained connection logs contained up to 17 concurrent requests in PID
  2793741, 89 cancelled requests, and a maximum elapsed duration of 3,770,132 ms
  (about 63 minutes). That process had a start record but no normal stop record.
  This does not distinguish a crash, forced termination or lost final logging.
- PID 3910138 was running the release binary from the main CatDesk checkout.
  Its sampled resident high-water mark was 1,014,836 KiB. Samples of current RSS
  varied substantially; the process had not necessarily leaked all of that RAM.
- A worker was in kernel wait `p9_client_rpc`. Open file descriptors included
  `F_ARCHIVE/.Spotlight-V100/Store-V2/...`; the archive is a separate 9p mount.
  This proves archive traversal, but not which request initiated it.
- The host initially had about 16 GiB available RAM and about 10 GiB occupied
  swap. A later pressure sample reported memory `full avg10=13.17`. The queried
  kernel/systemd-oomd journal contained no OOM record in the preceding three days.
  RAM pressure is evidenced; an OOM kill is not established.
- The newer PID had no records in the primary connection log. At the time of
  investigation it had no diagnostics log FD. The old single-writer startup path
  could disable diagnostics entirely during overlapping restarts; this is a
  plausible explanation, not proof of the original startup error.
- The running process exited before its startup working directory/environment
  could be captured. No request arguments or secrets were extracted.

## Code defects and causal paths

1. `start_command` and some `run_command` calls take automatic before/after
   change snapshots. Recursive discovery did not stop at mount boundaries.
   The configured workspace defaults to the startup cwd unless `WORKSPACE_ROOT`
   overrides it. A broad workspace/cwd can therefore include an archive mount.
   Recursive listing/search is a separate possible traversal source; old logs
   do not identify the tool, so the exact historical initiating call is unknown.
2. Snapshot capture used `fs::read` for the entire file before retaining a
   128-KiB preview. Parallel requests multiplied transient allocation and disk
   I/O. Streaming now preserves full-content change detection with bounded
   per-file working buffers; it still reads the full file and can remain slow.
3. HTTP handlers executed synchronous filesystem work and tool processing on
   Tokio's async workers. Slow disks/commands can starve unrelated I/O. A bounded
   blocking pool now isolates this work, with no waiting admission queue and a
   response deadline. Slots survive client cancellation/timeouts until real
   completion. A timeout cannot cancel a kernel-blocked synchronous syscall.
4. The DevTools bridge wrote/flushed a request before registering its response
   channel. It could lose a fast response. EOF did not wake pending calls, and
   cancellation/timeouts retained their senders. Reusing IDs could also associate
   a late response with a subsequent call. Registration now precedes sending,
   internal IDs are unique, and pending state is cleaned on cancellation/EOF.
5. Browser mutex waits and pipe writes had no deadline; stdout lines and the UI
   event channel had no size/capacity bound. These are now bounded. EOF or
   oversized peer output closes the reader and fails outstanding calls; browser
   actions are not automatically replayed.
   Previously discarded stderr is now drained in bounded chunks and classified
   as memory/connection/other errors without persisting its potentially sensitive
   text. These categories aid diagnosis but do not prove a kernel OOM kill.
6. Dashboard rendering waited on shared application state before reading quit.
   It now skips a busy frame and continues keyboard polling. Cleanup and runtime
   shutdown have deadlines so blocked background work need not prevent exit.

## Verification and scope

Regression tests reproduced runtime timer starvation, ineffective response
deadlines, pending sender retention after cancellation, failure to wake on EOF,
unbounded snapshot read buffers, ping waiting on a busy state lock, and logging
loss during overlapping process startup before the corresponding fixes.

Additional checks cover immediate peer replies and ID preservation, bounded
browser lock waits, capacity retained after a client disconnect, full-file tail
change detection beyond the preview, existing MCP protocol behavior, command job
durability/cancellation, and diagnostic redaction/rotation. Test results are
reported with the final change rather than frozen as a count in this document.

No hard host/cgroup memory cap was installed, no OOM exemption was granted, and
no running CatDesk process was killed by this investigation. Application limits
reduce the confirmed pressure paths but do not bound arbitrary child processes
or every operation that reads/edits a large file. Host disk faults, browser
failures and ngrok reconnections can still require separate diagnosis.
