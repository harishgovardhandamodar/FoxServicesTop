# Fox-Services-Top User Guide

Terminal monitor for NVIDIA GPUs on Linux and DGX Spark — plus your whole
fox-services cluster: per-node health on the **Nodes** tab and every Docker
service on the **Docker** tab.

## Install

```bash
curl -fsSL https://raw.githubusercontent.com/harishgovardhandamodar/FoxServicesTop/master/install.sh | bash
```

Linux x86_64 or aarch64 with glibc 2.34+ (Ubuntu 22.04, Debian 12, RHEL 9+;
DGX Spark included; WSL 2 works). GPU metrics need the NVIDIA driver —
without it, CPU, memory, disks and network still work.

Or build from source (Rust 1.92.0, pinned by `rust-toolchain.toml`) — one
command with the source installer, which also has `--service` to set up the
headless collector below and `--install-rust` to add rustup first:

```bash
curl -fsSL https://raw.githubusercontent.com/harishgovardhandamodar/FoxServicesTop/master/install-from-source.sh | bash
```

Or by hand:

```bash
git clone https://github.com/harishgovardhandamodar/FoxServicesTop.git
cd FoxServicesTop
cargo build --release --locked
./target/release/fox-services-top
```

## Quick start

```bash
fox-services-top                       # live monitor
fox-services-top -i 0.5                # sample every 0.5 s for this run only
fox-services-top -t amber              # theme for this run: dark, cyan, green, amber
fox-services-top --no-gpu              # do not start the NVML host
fox-services-top --read-only           # no process actions
fox-services-top --daemon              # headless collector; viewers attach to it (see below)
fox-services-top --check-config        # validate config, print saved/effective values
fox-services-top --diagnostics-json    # versions, SQLite, capabilities, health as JSON
```

CPU power (the ⚡ figures) comes from the Linux powercap/RAPL counters where the
hardware exposes them. The kernel makes those counters root-only, so a normal
run shows no CPU power and no CPU + GPU total even when GPU power is available.
Grant read access once with the udev rule below (no `sudo` for the monitor
itself), or run the whole monitor as root — `sudo -H fox-services-top` — in
which case process actions (`K`/`F9`) are disabled.

## CPU power without root

`energy_uj` under `/sys/class/powercap` is mode `0400 root:root` since Linux
5.10 (the counters can feed a side-channel attack). The collector reports that
as `permission_denied` — never as `0` — and leaves the CPU power and the total
hidden. To read them as your normal user, install the shipped udev rule once:

```bash
# With the source installer (from the repository directory):
./install-from-source.sh --cpu-power

# Or by hand, from a checkout:
sudo cp packaging/udev/60-fox-services-top-powercap.rules /etc/udev/rules.d/
sudo udevadm control --reload-rules
sudo udevadm trigger --subsystem-match=powercap
```

The rule runs `chmod a+r` on each `energy_uj` as its zone registers. It changes
nothing else: the control files stay root-only and nothing becomes writable. A
running collector re-probes a denied counter by itself, so it picks the reading
up without a restart; the ⚡ figures appear within a couple of intervals. On a
shared machine where untrusted local users must not be able to attempt the side
channel, skip this and run the collector as root instead. Remove the file and
re-run the two `udevadm` commands to undo it.

The hardware must still expose the counters: a host with no powercap zone (and
no `spbm` hwmon power sensor) has no CPU power to read, rule or not.

## Background collector (`--daemon`)

This is the short version; [The Headless Collector](COLLECTOR.md) is the
end-to-end guide — installing the service, the viewer lifecycle, verifying that
it records while nothing is shown, and the exact error messages.

By default fox-services-top collects and draws in one process, so history only
accumulates while a window is open. `--daemon` runs the collection and
persistence with **no terminal**, and any viewer attaches to it:

```bash
fox-services-top --daemon      # headless collector (foreground; add `&` to background it)
fox-services-top               # attaches to the collector and draws, or runs standalone if none
```

- The collector runs the ordinary pipeline (sampling, source selection, state,
  history, the SQLite databases) with no terminal. It opens **one** session and
  serves it to **one viewer at a time** over a Unix socket in the state
  directory (`attach.sock`; `0600` inside the `0700` state directory). It keeps
  collecting whether or not a viewer is attached, so the Overview History and
  Process History span the whole time it runs, not only the time a window was
  open.
- A plain `fox-services-top` connects to that socket when it is present and
  becomes the **viewer**: the same tabs, keys and process actions, driven by
  the collector's live session, with the full on-disk history. Quitting the
  viewer (`q` / `Ctrl-C`) only detaches it — the collector keeps running — and
  the viewer exits by itself when the collector stops.
- With no collector running, a plain run collects and draws in one process,
  exactly as before. `--no-attach` forces that standalone run even when a
  collector *is* listening (it then fails if the collector holds the directory's
  `instance.lock`).
- One collector per state directory: the instance lock admits a single writer.
  Use `--state-dir` for a second, separate instance.
- A viewer whose attach protocol differs from the collector's refuses to attach
  and says so; restart the collector to match.

### Install it as a systemd user service

A systemd **user** service starts the collector on login, or at boot with
lingering enabled — no root and no terminal. The unit template ships at
[`packaging/systemd/fox-services-top.service`](../packaging/systemd/fox-services-top.service):

```bash
mkdir -p ~/.config/systemd/user
curl -fsSL https://raw.githubusercontent.com/harishgovardhandamodar/FoxServicesTop/master/packaging/systemd/fox-services-top.service \
  -o ~/.config/systemd/user/fox-services-top.service
# If the binary is not in ~/.local/bin, edit ExecStart= in that file first.

systemctl --user daemon-reload
systemctl --user enable --now fox-services-top.service
loginctl enable-linger "$USER"     # start at boot, before you log in
```

Then open a viewer whenever you want one — it attaches to the service:

```bash
fox-services-top
```

Manage the service:

```bash
systemctl --user status fox-services-top
systemctl --user restart fox-services-top
systemctl --user stop fox-services-top
systemctl --user disable --now fox-services-top
journalctl --user -u fox-services-top -f      # what systemd sees
```

The unit runs the collector as your user, so `sudo`-only CPU power counters are
not read; the app's own log (see the file table below) still records what it
does. If you installed the binary elsewhere, set `ExecStart=` to its absolute
path. `install.sh` does not install the unit.

## Tabs and keys

| Key | Action |
|---|---|
| `1`–`7`, `Tab`, `Shift-Tab` | Overview, GPU, Overview History, Process History, Settings, Nodes, Docker |
| `q`, `Ctrl-C` | Quit (`q` is text while typing a filter) |
| `?`, `F1` | Help |
| `+` / `-` | Faster / slower sampling |
| `j`/`k`, arrows, `PgUp`/`PgDn`, `Home`/`End` | Move the selection (scroll on Nodes/Docker) |
| `/` | Filter (`Enter` applies, `Esc` clears; narrows Nodes/Fox-Services tables too) |
| `c` | Per-core CPU bars and sensors |
| `[` / `]` | Scroll the memory panel's GPU processes |
| `s` / `r` | Next sort column / reverse order (process table) |
| `g` | Cycle the GPU filter |
| `K` / `F9` | SIGTERM / SIGKILL the selected process (asks first) |
| `y`/`Enter`, `n`/`Esc` | Confirm / cancel |
| `h`/`l` | Previous / next GPU in the GPU view |
| `w`/`W` | Next / previous history bar size (1m … 24h) |
| `Enter`, `e`, `y` | Open a run / expand its command line / copy it (Process History) |
| `Space` | Enable / disable a module (Settings) |

Each panel also names its own keys at the right of its title, and tabs are
clickable with the mouse.

## The local tabs (1–5)

- **Overview** — every GPU (utilization, memory, temperature, power), CPU,
  memory (split by GPU-process share on unified memory), busiest disks and
  network interfaces, and the GPU process table in per-process colors. The
  process panel has a second sub-tab, **Fox services**, showing the
  Fox-Services tables in place — click it (or the `GPU Processes` sub-tab
  to go back).
- **GPU** — one GPU at a time with full detail: clocks, fans, PCIe, power
  limits, throttle reasons, firmware and versions.
- **Overview History** — bars of 1 minute to 24 hours for CPU, memory, GPUs,
  disks and network, kept on disk across restarts.
- **Process History** — every GPU process as a run with runtime, peaks and
  how it ended; open a run for its command line, folder and charts.
- **Settings** — sampling interval, display modules and data-source health.
  Each setting shows saved vs effective value, origin, and whether it applies
  now or after a restart.

## The Nodes tab (6)

One row per configured fox-services node with its latest polled snapshot:

- reachability (`online`, or the error when it does not answer),
- CPU %, memory %, GPU kind + utilization, network RX/TX,
- LLM token throughput, running/total container counts,
- Fox Mesh peers the nodes discovered, with which node reported them.

`/` narrows the table by node name. The view is read-only and never blocks
the local monitor: a dead node just keeps its error row.

<img src="images/screenshots/workstation-nodes.png" alt="Nodes tab: axiom online with CPU, memory, GPU, network, token and container readings, axiom-dgx offline, and one mesh peer" width="100%">

## The Fox-Services tab (7)

Live: which app uses which LLM model, per request — aggregated from actual
LLM requests over `remote.service_window_hours` (24 h default), not compose
config. The node matches services to compose projects itself and reports
each service's status.

- service table: service, status (`Busy` / `Live` / `Running` /
  `Unreachable` / `stopped` / `Not deployed`), the models it used with real
  request and token counts, totals, last request,
- recent requests below: time, service, model, tokens (prompt/completion/
  total), duration with queue wait, content type, prompt snippet.

Read-only — it only watches. `/` filters both tables by service or model;
`j`/`k` scroll the recent requests.

<img src="images/screenshots/workstation-services.png" alt="Fox-Services tab: live service-to-model table with request and token counts, then recent LLM requests with attribution" width="100%">

## Monitoring your nodes

Out of the box, fox-services-top polls the example cluster:

- `axiom` → `http://192.168.1.63:8210`
- `axiom-dgx` → `http://192.168.1.200:8210`

Each node just needs fox-services running (port 8210 by default). The poller
only reads fast endpoints — `/api/hardware/status`, `/api/llm/live-map`,
`/api/usage/history`, `/api/ollama/stats`, `/api/docker/projects` and, with
mesh discovery on, `/api/mesh/peers` — every 5 seconds, on its own thread,
with a 4-second per-request timeout.

To point at your own nodes, edit the `remote` section of `config.json`
(changes need a restart):

```json
"remote": {
  "enabled": true,
  "poll_interval_ms": 5000,
  "timeout_ms": 4000,
  "discover_mesh": true,
  "nodes": [
    {"name": "axiom", "base_url": "http://192.168.1.63:8210"},
    {"name": "axiom-dgx", "base_url": "http://192.168.1.200:8210"}
  ]
}
```

At most 16 nodes, plain HTTP. For one run only, without touching the file:

```bash
fox-services-top --remote-node axiom=http://192.168.1.63:8210 --remote-node lab=http://10.0.0.9:8210
fox-services-top --no-remote    # local monitor only
```

Mesh peers a node reports appear under the node table automatically when
`discover_mesh` is on; pairing between the fox-services nodes themselves is
managed in fox-services, not here.

## Configuration and files

| Purpose | Location |
|---|---|
| Configuration | `$XDG_CONFIG_HOME/fox-services-top/v2/config.json` (`~/.config/...`) |
| Overview History bars (~40 MiB) | `$XDG_STATE_HOME/fox-services-top/v2/history_tiers.sqlite3` |
| Recent minutes (7 days, ≤ 1 GiB) | `.../history.sqlite3` |
| Process History | `.../process_history.sqlite3` + `process_recent.json` |
| Action audit log (30 days) | `.../audit.sqlite3` |
| Log file, instance lock | `.../fox-services-top.log`, `instance.lock` |
| Collector attach socket | `.../attach.sock` (a `--daemon` collector; viewer only) |

`--config` / `--state-dir` take absolute paths. Directories are `0700`,
files `0600`. Databases are never migrated: a foreign format starts empty,
`config.json` is always kept.

## Troubleshooting

- **A node shows offline.** Check the error in its row (timeout, refused,
  bad status), then `curl http://<host>:8210/api/hardware/status` from this
  machine. LAN firewalls and wrong ports are the usual cause.
- **Empty Nodes/Fox-Services tabs.** `remote.enabled` may be off, or every node
  unreachable — the tabs say which. `--check-config` validates the section.
- **Tabs missing.** The `cluster` / `docker` modules can be toggled in
  Settings like any display module; both are on by default.
- **Slow shutdown.** The poller is stopped inside the 3 s shutdown budget; a
  node hanging exactly at timeout can add a warning line, never a hang.
- **No ⚡ CPU power or total.** Linux powercap/RAPL counters are root-only, so a
  normal run reads none; install the udev rule under "CPU power without root"
  above (`install-from-source.sh --cpu-power`). If the host has no powercap
  zone at all, there is no CPU power to read.
- **History only covers the time a window was open.** Run `--daemon` (or install
  the service above) so the collector keeps recording while nothing is shown; a
  plain run then attaches to it.
- **`a collector is running with attach protocol vN`.** The viewer and the
  collector are different builds. Restart the collector, or start the viewer
  with `--no-attach` to run standalone.
- **`instance.lock` is held.** A collector is already running for this state
  directory. Attach to it with a plain run, stop it, or use another
  `--state-dir`.
- **Something looks wrong.** `fox-services-top --diagnostics-json` prints
  versions and health; the log file (see table above) records what happened.
