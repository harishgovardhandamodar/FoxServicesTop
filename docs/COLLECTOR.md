# The Headless Collector

Run collection and persistence in the background with no window open, and open
a viewer whenever you want one. This is the end-to-end guide: install it,
attach to it, check that it is really recording, and fix it when it misbehaves.
(The [User Guide](USER_GUIDE.md) has the short version and the rest of the
app.)

## The idea

Normally fox-services-top collects and draws in one process, so history only
covers the time a window was open. `--daemon` splits the two halves:

- the **collector** keeps the ordinary pipeline running — sampling, source
  selection, state, Overview History, Process History, the SQLite databases —
  with no terminal at all;
- a **viewer** is a plain `fox-services-top` that connects to the collector's
  Unix socket and draws its live session, exactly like a standalone window but
  without doing any collecting itself.

Quitting the viewer only detaches it; the collector keeps recording. The record
therefore spans the whole time the collector runs, not the times you happened
to have a window open.

Two rules hold everything together:

- **One collector per state directory.** An `instance.lock` admits a single
  writer. Use `--state-dir` for a second, independent instance.
- **One viewer at a time.** The collector serves one connection, then accepts
  the next.

The socket lives at `$XDG_STATE_HOME/fox-services-top/v2/attach.sock`
(`~/.local/state/fox-services-top/v2/attach.sock` by default), mode `0600`
inside the `0700` state directory — only your own user can attach.

## Option A: a user service (recommended)

On Linux that is a systemd **user** service; on macOS a launchd agent. The
templates ship at
[`packaging/systemd/fox-services-top.service`](../packaging/systemd/fox-services-top.service)
and
[`packaging/macos/me.harishgovardhandamodar.fox-services-top.plist`](../packaging/macos/me.harishgovardhandamodar.fox-services-top.plist).
They start the collector on login. No root, no terminal.

The Linux unit runs `%h/.local/bin/fox-services-top --daemon` — `install.sh`'s
default location. If your binary lives elsewhere, edit `ExecStart=` first.
`install.sh` does not install this file; it is a manual template. The source
installer does it for you:

```bash
./install-from-source.sh --service     # build, install and enable the unit (Linux) or agent (macOS)
```

Or by hand:

```bash
mkdir -p ~/.config/systemd/user
curl -fsSL https://raw.githubusercontent.com/harishgovardhandamodar/FoxServicesTop/master/packaging/systemd/fox-services-top.service \
  -o ~/.config/systemd/user/fox-services-top.service
```

```bash
systemctl --user daemon-reload
systemctl --user enable --now fox-services-top.service
loginctl enable-linger "$USER"     # start at boot, before any login
```

Lingering is the step people miss: without it the service starts when you log
in and stops when the last session ends, so it does not cover reboots.

Now open a viewer whenever you like — it just attaches:

```bash
fox-services-top
```

Manage the collector:

| Task | Command |
|---|---|
| Is it running? | `systemctl --user status fox-services-top` |
| Start | `systemctl --user start fox-services-top` |
| Stop | `systemctl --user stop fox-services-top` |
| Restart (e.g. after upgrading) | `systemctl --user restart fox-services-top` |
| Remove from startup | `systemctl --user disable --now fox-services-top` |
| What systemd sees | `journalctl --user -u fox-services-top -f` |

Stopping is a clean SIGTERM shutdown that exits `0`; the `Restart=on-failure`
policy revives the collector only after a crash (a non-zero exit), 5 s later.
systemd allows 15 s for the shutdown, which is more than the app's own 3 s
budget.

Because the service runs as your user, root-only counters are not read —
powercap / RAPL CPU power on Linux, `powermetrics` samplers on macOS — same
as any normal session. The app's own log,
`~/.local/state/fox-services-top/v2/fox-services-top.log`, records what the
collector does.

### On macOS: the launchd agent

```bash
mkdir -p ~/Library/LaunchAgents
curl -fsSL https://raw.githubusercontent.com/harishgovardhandamodar/FoxServicesTop/master/packaging/macos/me.harishgovardhandamodar.fox-services-top.plist \
  -o ~/Library/LaunchAgents/me.harishgovardhandamodar.fox-services-top.plist
# If the binary is not in ~/.local/bin, edit ProgramArguments in that file first.

launchctl bootstrap "gui/$(id -u)" ~/Library/LaunchAgents/me.harishgovardhandamodar.fox-services-top.plist
```

| Task | Command |
|---|---|
| Is it running? | `launchctl print "gui/$(id -u)/me.harishgovardhandamodar.fox-services-top"` |
| Stop | `launchctl bootout "gui/$(id -u)/me.harishgovardhandamodar.fox-services-top"` |
| Start again | `launchctl bootstrap "gui/$(id -u)" ~/Library/LaunchAgents/me.harishgovardhandamodar.fox-services-top.plist` |

The agent starts at login (`RunAtLoad` + `KeepAlive`) and appends the
collector's output to `~/Library/Logs/fox-services-top.log`.
`install.sh` does not install the agent.

## Option B: run it by hand

```bash
fox-services-top --daemon      # foreground, no terminal UI
fox-services-top --daemon &    # background it from your shell
```

Stop it with `kill -TERM <pid>` (or `Ctrl-C` while it is in the foreground); it
performs its bounded shutdown and exits `0`. For a hands-off session, run it
inside `tmux`/`screen` or under `nohup`.

`--daemon` is refused together with `--replay` (exit `2`): replay generates its
own data in a temporary state directory and is pointless as a service.

## Using a viewer

```bash
fox-services-top          # attaches if a collector is listening
```

There is no `--attach` flag to remember: a plain run **auto-attaches** when the
socket connects and the handshake succeeds, and otherwise falls back to the
standalone behaviour of running collection and UI in one process.

What differs from a standalone window:

- **The data comes from the collector.** The sample interval, GPUs, read-only
  mode and every other collection setting are whatever the collector was
  started with; on the viewer those flags only affect how the window draws.
- **Everything else is the same.** All seven tabs, the key map, process actions
  (`K` / `F9`), Settings — the commands are applied to the collector's live
  session, so actions take effect on the running collector and land in its
  audit log.
- **`q` / `Ctrl-C` detaches.** The collector keeps running and keeps recording.
- **The viewer exits by itself when the collector stops**, because the
  collector tells it to on the way down.

Force a standalone run (never touch the socket) with `--no-attach`. Note that a
standalone run still needs a free `instance.lock`, so it fails while a collector
holds it — `--no-attach` is for running against a *different* `--state-dir`, or
for working around a handshake problem. `--replay` also never attaches.

### Only one viewer at a time

If a viewer is already attached, a second one connects to the socket and then
waits for the collector to become free. It waits up to **5 seconds**; if the
first viewer is still open it exits `1` with:

```text
attaching to the running collector: the collector did not answer the attach
handshake
```

Close the first viewer (`q`) and start again — the collector accepts the next
viewer as soon as a connection ends.

## Check that it is really recording

```bash
ls -l ~/.local/state/fox-services-top/v2/attach.sock   # socket exists (0600)
systemctl --user is-active fox-services-top            # "active"
journalctl --user -u fox-services-top -n 20 --no-pager # startup, no errors
```

Then the proof that matters: open a viewer, note a metric now, wait a few
minutes with the viewer closed (or stop it entirely), reopen it, and look at
**Overview History** and **Process History** — the bars and runs from the
closed period are there, because the collector wrote them while nothing was
shown.

## Flag reference

| Flag | Effect |
|---|---|
| `--daemon` | Run headless: collect and persist with no terminal, serve viewers on the attach socket. |
| `--no-attach` | Never attach to a collector; force a standalone run (fails if the collector holds the lock). |
| `--state-dir <dir>` | A separate instance in another state directory — its own socket, lock, databases and logs. Pass the **same** `--state-dir` to a viewer to attach to that collector. |
| `--replay <file>` | Simulated data, read-only, temporary state; never attaches and cannot be combined with `--daemon` (exit `2`). |

Exit codes: `0` success (including a viewer that detached cleanly and a
collector stopped by SIGTERM), `1` runtime failure (handshake, socket, fail-stop),
`2` invalid invocation.

## Files it touches

| Purpose | Location |
|---|---|
| Attach socket (viewers connect here) | `.../v2/attach.sock` (`0600`) |
| Single-collector lock | `.../v2/instance.lock` |
| Collector log | `.../v2/fox-services-top.log` |
| Overview / recent / Process History | `.../v2/history_tiers.sqlite3`, `history.sqlite3`, `process_history.sqlite3` |
| Action audit log | `.../v2/audit.sqlite3` |

Directories are `0700`, files `0600`; `...` is `$XDG_STATE_HOME/fox-services-top`
(`~/.local/state/fox-services-top` by default).

## Troubleshooting

Errors print as `fox-services-top: <message>` on stderr.

- **`another fox-services-top instance is already running on
  <state-dir>/instance.lock (pid N)`** (exit `1`). One collector (or standalone
  run) already owns this state directory — the single-writer rule. Attach with
  a plain run, stop the other instance, or start this one with its own
  `--state-dir`.
- **`a collector is running with attach protocol vN; this build speaks vM;
  restart the collector (or pass --no-attach to force a standalone run)`**
  (exit `1`). The viewer and the collector are different builds. Restart the
  collector (`systemctl --user restart fox-services-top`), or run the viewer
  with `--no-attach`.
- **`attaching to the running collector: the collector did not answer the
  attach handshake`** (exit `1`). A viewer is already attached (close it, then
  retry), or the collector is unresponsive — check
  `journalctl --user -u fox-services-top -f`, then restart the service.
- **`attach socket <path>: <error>`** (exit `1`). The collector could not
  create its socket at startup — in practice a permissions or ownership
  problem in the state directory, which must be owned by your user and `0700`.
  A leftover socket file is removed automatically at startup, so you never need
  to delete one by hand.
- **After a crash the window just runs standalone.** A killed collector leaves
  its socket file behind, but the next plain run gets a refused connection and
  falls back to a standalone run (which takes the now-free lock) — nothing is
  wrong, but nothing is being recorded in the background either. Start the
  service again to get a collector back.
- **The service does not start at boot.** Lingering is off: run
  `loginctl enable-linger "$USER"` and check `systemctl --user status`.
- **`fox-services-top --diagnostics-json`** prints versions and health if you
  need to report a problem.

## See also

- [User Guide](USER_GUIDE.md) — all tabs and keys, configuration, general troubleshooting.
- [`packaging/systemd/fox-services-top.service`](../packaging/systemd/fox-services-top.service) — the unit template.
- [Design decisions](design/DECISIONS.md) — D-107 records why the collector is a
  separate process attached over a local socket.
