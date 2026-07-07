# RMNG

![RMNG](docs/hero.webp)

> **Self-hosted GPU Linux desktops for supervised AI-agent fleets.**

A self-contained Rust system for running, viewing, and automating a fleet of containerized GNOME desktops. A single **control-server** container orchestrates **clone containers** on a local Docker daemon, hardware-encodes the selected clone's GPU frames to a **native hardware-decode GTK viewer**, and brokers the desktop-automation **MCP** that per-clone Claude agents drive. Each clone runs a thin **clone-daemon** that captures frames, injects input, and bridges the clipboard.

## What problem does this solve?

AI coding agents are useful enough to run in parallel, but the supervision surface is still mostly one-at-a-time. Terminal orchestrators can multiplex text panes, IDE agents usually live inside a single editor session, and cloud agents often hide the live desktop state behind a managed workflow. That is a poor fit when the task needs a real browser, GUI application, desktop login state, or a human to take over the mouse at the exact point where the agent gets stuck.

RMNG treats each agent as a full desktop workload instead of only a shell process. Every clone gets its own isolated GNOME session with browsers, GUI apps, account state, clipboard, SSH, SMB home access, and local automation APIs. The operator gets one control plane for creating clones, assigning Claude/Codex accounts, watching usage, chatting with per-clone agents, switching the native viewer between desktops, and taking over input without restarting the work.

The intended use case is one technical operator supervising many long-running agent desktops on infrastructure they control. The important properties are:

- **Parallel supervision:** many clones can run independently while the dashboard, viewer, notes, state, and "needs human" detector route attention to the clone that needs intervention.
- **Real desktop state:** agents can use browsers, IDEs, OAuth sessions, GUI tools, screenshots, and clipboard flows that do not fit a terminal-only loop.
- **Fast human takeover:** the viewer is a native hardware-decode client with multi-monitor support and instant clone switching, not a browser tab per desktop.
- **Self-hosted control boundary:** code, repo checkouts, browser profiles, and harvested account tokens stay on the operator's Docker/GPU host rather than inside a managed cloud-agent vendor.
- **Agent-native plumbing:** per-clone MCP surfaces, the `rmng desktop` CLI, hot account swaps, account groups, usage polling, and sticky auto-rotation are part of the runtime instead of external scripts around a VDI product.

## How does it compare?

RMNG overlaps with several categories, but it is not a drop-in replacement for all of them. It is heavier than terminal tools and less turnkey than hosted products because it runs real desktops on your own GPU host.

| Category | Good fit | Where RMNG differs |
|---|---|---|
| Terminal agent orchestrators (for example Conductor, Async) | Running many shell/git agents with text logs and patches | RMNG adds a full Linux desktop per agent, GUI/browser state, native visual takeover, and per-desktop automation. It is heavier because every agent is a desktop container, not just a process. |
| IDE background agents (Cursor, Copilot) | Working inside one editor workspace | RMNG can run IDEs inside clones, but its control unit is the clone: one isolated desktop, repo, browser profile, account assignment, and agent loop per task. |
| Managed cloud agents (for example Devin/Cognition) | Outsourcing the runtime and workflow to a hosted service | RMNG is self-hosted and inspectable. You can watch and take over the live desktop, but you operate the Docker host, GPU path, updates, and credentials yourself. |
| Agent desktop infrastructure (for example Scrapybara, Bytebot) | Hosted or API-driven desktops for agents | RMNG is focused on operator supervision of a local fleet: native viewer, dashboard state, clone notes, needs-human routing, account lifecycle, and direct desktop takeover. |
| Cloud desktop / VDI systems (for example Kasm) | Human remote desktops, browser/WebRTC access, existing VDI management | RMNG is not a general VDI suite. It builds the media path, clone lifecycle, and MCP/agent integration around AI-agent workloads rather than human office desktops. |

## Architecture

One central encoder feeds both the viewer and the agents' screenshots; raw H.264 over TCP into zero-copy VA-API decode gives RFX-class feel without RDP, and media/input cross a host unix socket so only the control-server is exposed. Full port map, protocols, and workspace layout: **[docs/DEVELOPMENT.md](docs/DEVELOPMENT.md)**.

**Built from scratch** — encoder, decoder, viewer, and wire protocol are all original Rust; no code from VNC, RDP, or gnome-remote-desktop.

## Features

**Remote Desktop**

- Zero-copy full-chroma 4:4:4 hardware H.264 video pipeline end to end (even on hardware that only supports 4:2:0!)
- Native hardware-accelerated viewer on Linux and macOS
- Full 60fps in local network
- Multi-monitor
- Instant swap between clones
- Absolute + relative pointer
- Rich clipboard bridge, remote↔remote and local↔remote
- Port forwarding

**Fleet Management**

- One control-server Docker container manages everything, and is the access point to everything
- Web-based control dashboard, note for each clone
- Docker + lxcfs for clone sandboxing
- Full GNOME in each clone
- Central SMB share for every clone's home dir
- One command to SSH into every clone

**Agent Native**

- `rmng` fleet management CLI in every clone (hosts, clones, images, accounts — over the control-server web API)
- Fleet-wide desktop automation through the `rmng desktop` CLI, backed by each clone's daemon MCP on localhost:9004
- Chat with per-clone agent over web UI
- "Needs human" detector

**Accounts & integrations**

- Import Claude and Codex/ChatGPT accounts once from a signed-in clone; the server owns token refresh after import
- 5h + 7d usage visualizer for all accounts, including stale and rate-limited state
- Live hot-swap of a running clone's account, no restart
- Named account groups with sticky auto-rotation
- Pin a clone, or bind it to auto / a group / none

## Quick start

> **Hardware support:** the **encode** path (control-server, VA-API H.264) has only been tested on an **AMD Radeon Pro W6800**; the **decode** path (viewer) has only been tested on **Intel integrated graphics** (Linux) and **Apple M-series** (macOS). Other GPUs may work but are untested.

Needs a Linux host with Docker and a GPU render node (`/dev/dri/renderD128`). Pull the published image (or `docker build -t rmng:latest .` from a checkout), then run the hub:

```sh
docker run -d --name rmng --privileged --init --pid host --restart unless-stopped \
  -v /var/run/docker.sock:/var/run/docker.sock \
  -v rmng-data:/data -v rmng-sock:/srv/rmng-sock \
  -p 9000-9002:9000-9002 -p 9005:9005 -p 445:445 -p 2222:2222 pegasis0/rmng
```

Ports: `9000` web UI/API · `9001` video · `9002` per-clone MCP · `9005` port-forward data plane · `445` SMB clone-home share (host `445` must be free) · `2222` SSH bastion (jump into clones).

Open `http://<host>:9000` → the **first-run setup wizard** (environment checklist → server settings → download the clone template → finish) does the rest; then **Settings** for Linear/Claude credentials. There are zero `-e` config flags — everything is set in the UI. Full flow, the image build, publishing the template, upgrades, and the dev loop: [docs/DEPLOY.md](docs/DEPLOY.md). Running the Docker host on a Proxmox LXC CT: [docs/PROXMOX-LXC.md](docs/PROXMOX-LXC.md).

## Documentation

Architecture, the full port/protocol map, the workspace layout, build prerequisites, and the clean-room policy live in **[docs/DEVELOPMENT.md](docs/DEVELOPMENT.md)**, which also links every deep-reference doc ([API](docs/API.md) · [CLI](docs/CLI.md) · [MCP](docs/MCP.md) · [PROTOCOL](docs/PROTOCOL.md) · [SCRIPTS](docs/SCRIPTS.md) · [DEPLOY](docs/DEPLOY.md) · [PROXMOX-LXC](docs/PROXMOX-LXC.md)).
