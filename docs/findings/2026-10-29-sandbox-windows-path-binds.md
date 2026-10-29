# Sandbox launch cost of Windows PATH binds — 2026-10-29

## Problem

Production logs (pid 2208099, 2026-10-28 22:27 .. 10-29 13:22, 366
`run_command` calls) showed a floor of ~100 ms per command: the cheapest
`ls`/`git status` calls took 104 ms minimum, 169 ms median, while the same
`git status` runs in 10 ms on the host.

`runtime_read_paths` read-only binds every `PATH` directory into the
bubblewrap namespace. Under WSL, `PATH` carries four Windows directories on
the `9p` drive mount (`/mnt/c/Windows`, `/mnt/c/Windows/System32`,
`…/Microsoft VS Code/bin`, `…/WindowsApps`). Canonicalizing and mounting them
crosses 9p on every command.

Measured through `run_shell_command` with the real sandbox
(`systemd-run --scope` + `bwrap`, 12 runs each):

| case | `true` p50 | `git status` p50 | argv preparation |
|---|---|---|---|
| PATH with `/mnt/c` entries | 120 ms | 122–131 ms | 22–26 ms |
| PATH without `/mnt/c` entries | 22–24 ms | 26–28 ms | ~0 ms |
| after the change (real PATH) | 13 ms | 16 ms | — |

Component costs on the same host: `systemd-run --user --scope true` ~10 ms,
plain `bwrap` ~21 ms, `bash -c true` ~9 ms.

The binds bought nothing: Windows executables cannot run in the sandbox.
`cmd.exe /c echo …` and `/mnt/c/Windows/System32/wsl.exe --version` both exit 1
with `WSL … UtilConnectUnix:543: connect failed` — interop needs the host's
`/init` as PID 1 and the `/run/WSL` socket, neither present in the namespace.

## Change

`PATH` entries on a WSL Windows drive are no longer bound. Drives are read
once per process from `/proc/self/mountinfo`: `9p`/`drvfs`/`virtiofs` mounts
whose source is a drive letter (`C:\`). Other 9p mounts such as
`/usr/lib/wsl/drivers` and the `/usr/lib/wsl/lib` overlay (GPU libraries) are
unaffected. An entry is checked before canonicalizing (to skip the 9p lookup)
and after (so a symlink into a drive is dropped too). Outside WSL nothing
matches and `PATH` is bound as before.

The `wsl.exe --shutdown` guard in `command_policy` stays: it runs before spawn
and costs nothing.
