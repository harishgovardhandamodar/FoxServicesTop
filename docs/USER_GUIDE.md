# Fox-Services-Top User Guide

Terminal monitor for NVIDIA GPUs on Linux and DGX Spark — plus your whole
fox-services cluster: per-node health on the **Nodes** tab and every Docker
service on the **Docker** tab.

## Install

```bash
curl -fsSL https://raw.githubusercontent.com/harishgovardhandamodar/fox-services-top/main/install.sh | bash
```

Linux x86_64 or aarch64 with glibc 2.34+ (Ubuntu 22.04, Debian 12, RHEL 9+;
DGX Spark included; WSL 2 works). GPU metrics need the NVIDIA driver —
without it, CPU, memory, disks and network still work.

Or build from source (Rust 1.92.0, pinned by `rust-toolchain.toml`):

```bash
git clone https://github.com/harishgovardhandamodar/fox-services-top.git
cd fox-services-top
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
fox-services-top --check-config        # validate config, print saved/effective values
fox-services-top --diagnostics-json    # versions, SQLite, capabilities, health as JSON
```

Run with `sudo -H` to also read root-only CPU power counters (powercap/RAPL);
process actions (`K`/`F9`) are disabled in root sessions.

## Tabs and keys

| Key | Action |
|---|---|
| `1`–`7`, `Tab`, `Shift-Tab` | Overview, GPU, Overview History, Process History, Settings, Nodes, Docker |
| `q`, `Ctrl-C` | Quit (`q` is text while typing a filter) |
| `?`, `F1` | Help |
| `+` / `-` | Faster / slower sampling |
| `j`/`k`, arrows, `PgUp`/`PgDn`, `Home`/`End` | Move the selection (scroll on Nodes/Docker) |
| `/` | Filter (`Enter` applies, `Esc` clears; narrows Nodes/Docker tables too) |
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
  network interfaces, and the GPU process table in per-process colors.
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

## The Docker tab (7)

Every node's compose projects and containers in one place:

- project table: node, project, status (`running` / `stopped` /
  `not-deployed`), containers up/total, summed CPU % and memory,
- one sub-row per container: name, status, image, ports,
- read-only log tails per container below the table.

No start/stop/restart actions are offered — it only watches. `/` filters by
project, node or container name; `j`/`k` scroll the logs.

## Monitoring your nodes

Out of the box, fox-services-top polls the example cluster:

- `axiom` → `http://192.168.1.63:8210`
- `axiom-dgx` → `http://192.168.1.200:8210`

Each node just needs fox-services running (port 8210 by default). The poller
only reads four endpoints — `/api/hardware/status`, `/api/docker/stats`,
`/api/docker/logs/overview` and, with mesh discovery on, `/api/mesh/peers` —
every 5 seconds, on its own thread, with a 4-second per-request timeout.

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

`--config` / `--state-dir` take absolute paths. Directories are `0700`,
files `0600`. Databases are never migrated: a foreign format starts empty,
`config.json` is always kept.

## Troubleshooting

- **A node shows offline.** Check the error in its row (timeout, refused,
  bad status), then `curl http://<host>:8210/api/hardware/status` from this
  machine. LAN firewalls and wrong ports are the usual cause.
- **Empty Nodes/Docker tabs.** `remote.enabled` may be off, or every node
  unreachable — the tabs say which. `--check-config` validates the section.
- **Tabs missing.** The `cluster` / `docker` modules can be toggled in
  Settings like any display module; both are on by default.
- **Slow shutdown.** The poller is stopped inside the 3 s shutdown budget; a
  node hanging exactly at timeout can add a warning line, never a hang.
- **Something looks wrong.** `fox-services-top --diagnostics-json` prints
  versions and health; the log file (see table above) records what happened.
