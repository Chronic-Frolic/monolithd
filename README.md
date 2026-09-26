# monolithd

A lighting and machine-state daemon for one Linux HTPC and home server, White Monolith.
The case lights tell the room what the machine is doing: an ambient look when it's idle,
a progress bar for each real job (a Steam download, an rsync copy, a btrfs scrub), and
a warning or fault look when something needs attention. Every look is authored in
[QLC+](https://www.qlcplus.org/); `monolithd` only decides *which* look plays, from
truthful state, and carries QLC+'s output to the LEDs through
[OpenRGB](https://openrgb.org/).

**This is a personal project, built for one machine.** It runs on Bazzite (Fedora
Atomic) with KDE Plasma and Steam, and its QLC+ workspace and configuration describe
that machine's hardware: four RAM sticks, an ASUS ROG logo, an ARGB strip and a GPU
bracket. Read it, borrow from it, adapt it, starting with
[Requirements and assumptions](#requirements-and-assumptions). Issues and pull requests
are welcome, but there's no promise of support, and your hardware will need its own
workspace.

## What's in it

- **The controller** (`monolithd controller`): holds what every zone should show and
  reconciles the lights toward it. Other programs talk to it over a Unix socket.
- **Reporters** (`monolithd reporters`): Steam downloads read live from the Steam
  client's own download API, btrfs health and scrub progress, and a sweep that ends
  copy jobs whose process died.
- **Copy jobs** (`monolithd job rsync ARGS...`): an rsync copy as a job, with rsync's
  own overall progress as the bar.
- **Watchdog** (`monolithd watchdog`): holds off suspend while a job runs (capped at
  6 hours), and hands the lights over cleanly around sleep and wake.
- **Sleep policy** (`monolithd sleep-policy`): decides when the machine would suspend:
  a quiet period (2 hours by default) after the last job, input, gamepad use or music.
  It reads Wayland's input-idle protocol and evdev gamepads directly, and tells music
  from game audio. **It currently only observes and logs; it never suspends.**
- **Remote** (`monolithd remote`): a narrow HTTP API for suspend, reboot, power off,
  switching between Gaming and Desktop Mode, and a manual suspend block, behind a bearer
  token on loopback.

## Adding your own things

You don't need to touch the daemon. Anything that can write a line of JSON to the
controller socket can show a progress bar or raise a warning; from the shell it's
`monolithd event job-start ...`. [`docs/protocol.md`](docs/protocol.md) describes the
socket, every operation, and the rules that keep a bar truthful across restarts.

A new reporter built into the daemon follows the same rules through `src/reporter.rs`;
`src/steam.rs` and `src/storage.rs` are the worked examples.

## Layout

    src/        the Rust crate (one binary, many subcommands)
    config/     everything the binary reads at runtime (see below)
    qlcplus/    the production QLC+ workspace and the intake workspaces for authoring
    systemd/    user units (lighting stack, controller, watchdog, reporters, sleep policy), author mode;
                root/ holds the root scrub wrapper and its units
    python/     the read-only suspend-status tool and its test
    tools/      probe-header.sh and the intake template builder
    docs/       the controller protocol and the runbook

## Requirements and assumptions

- **The whole lighting stack is required, even for the parts that don't light
  anything.** The reporters and the sleep policy talk to the controller, which only
  starts when the QLC+ workspace and its registry agree, and which acts through the
  running lighting stack. There is no lights-free mode yet.
- **QLC+ and OpenRGB** as AppImages in `~/AppImages` (`MONOLITHD_QLCPLUS` and
  `MONOLITHD_OPENRGB` point elsewhere). QLC+ sends its output as E1.31 (sACN) to
  `monolithd e131-receiver`, which drives OpenRGB through its SDK. The receiver speaks
  the OpenRGB SDK itself (protocols 1 to 6; tested against OpenRGB 1.0).
- **Fixed ports** on loopback: 6742 (OpenRGB SDK) and 9999 (QLC+ web API) inside the
  lighting stack's private network namespace, where they can't clash with anything on
  the host; 8080 (Steam's DevTools) on the host.
- **Idle detection** needs a Wayland compositor with `ext_idle_notifier_v1`. KDE Plasma
  6 is measured; Gaming Mode's gamescope is not tested yet. Gamepads are read through
  evdev (`ID_INPUT_JOYSTICK` devices only, never keyboards).
- **Music detection** needs PipeWire with `pactl`. Streams from processes launched by
  Steam count as game audio, not music.
- **Storage health** is btrfs only. Device errors and missing devices come from
  `/sys/fs/btrfs`, unprivileged. Scrub progress and results need root: btrfs keeps them
  in a root-only file. So scrubs run through the root wrapper in `systemd/root/`, which
  mirrors their status to a world-readable file. Its header explains how to install it
  as a root-owned copy. Without it, a watched mount shows a "scrub state unreadable"
  warning once it has been scrubbed as root.
- **The Steam reporter needs Steam's DevTools port**, 127.0.0.1:8080, which
  [Decky Loader](https://github.com/SteamDeckHomebrew/decky-loader) opens. That port has
  no authentication: any local process can drive the Steam client through it, including
  its store and account pages. Enable it only on a machine where you trust every local
  process.
- **Monolith Remote** expects something in front of it that authenticates the network;
  the author uses Tailscale Serve. It must never listen on an open port.
- **systemd user services** and a checkout at `~/monolithd` (the units say so with `%h`).
- **A recent stable Rust** (the crate uses edition 2024). The author builds inside a
  toolbox container called `monolith-rust` because Bazzite's base image has no
  compiler; any Rust toolchain works.
- **`config/` is the author's own machine**: four ENE RAM sticks, an ASUS ROG STRIX
  B550-F with its ROG logo and one ARGB header, and a QLC+ workspace authored for them.
  Copy it and edit it for your hardware.

## Building and installing

    git clone https://github.com/Chronic-Frolic/monolithd ~/monolithd
    cd ~/monolithd && cargo build --release

Link the units you want from `systemd/` into `~/.config/systemd/user/`, then enable
them. Adapt the configuration and the QLC+ workspace to your hardware first;
`monolithd validate-registry` checks that they agree.

The configuration directory is, first match wins:

1. `$MONOLITHD_CONFIG_DIR`, when set (the shipped units set it to `~/monolithd/config`);
2. `~/.config/monolithd` (or `$XDG_CONFIG_HOME/monolithd`), when it exists;
3. the `config/` of the source tree the binary was built from.

Each service logs which one it uses. If you copy `config/` elsewhere, fix the relative
`workspace` path in `qlc-functions.toml`. Runtime state lives outside the repository:
sockets in `$XDG_RUNTIME_DIR/monolith-events/`, the Python venv and backups in
`~/.local/share/monolith-events/`.

## Configuration (`config/`)

- `scene-layout.toml`: zone identity and hardware geometry, the QLC+ and E1.31 listeners.
- `qlc-functions.toml`: registry of QLC+ Function IDs as workspace API (semantic name,
  ID, owned zones, composability, progress step ranges) plus per-zone DMX geometry.
  Its `workspace` path is relative to `config/`.
- `led-calibration.toml`: per-zone R/G/B gain applied at the very end of the output
  path. Scenes are authored in nominal color; hardware color correction lives only here.
- `controller.toml`: controller policy: the default ambient set, the order in which jobs
  take progress zones, and how long a finished job holds its bar (15 s).
- `reporters.toml`: one bar per service or one per item, and the btrfs mounts to watch.
- `watchdog.toml`: the zones the watchdog takes over, with the Controller Fault look while
  the controller is down and the quiet look around sleep.
- `sleep.toml`: the quiet period before the machine would suspend.

## Commands

    monolithd validate-registry              check the registry and calibration against the QLC+ workspace
    monolithd workspace select NAME [--check] import an owner-authored intake workspace
    monolithd scene status                   gateway state, zone owners, calibration in force, output health
    monolithd scene start-set A B C          start several zone functions in phase
    monolithd scene progress ZONE N          select progress step N for a zone
    monolithd scene replace|start|stop NAME  drive the scene gateway directly
    monolithd scene rejoin NAME              return a zone to its ambient function, in phase with the running ones
    monolithd controller                     run the controller (lighting state and job leases)
    monolithd event status                   controller state: ambient set, zones, jobs, recent actions
    monolithd event job-start ID LABEL TOTAL announce a job (then job-progress, job-complete, job-fail)
    monolithd event pause | resume           stop or resume the controller acting on the plant
    monolithd calibrate --show               list the gains and what full white becomes
    monolithd calibrate ZONE R G B           set a zone gain (0.0 to 1.0), applied within a second
    monolithd reporters [--dry-run]          the reporters service: Steam downloads, storage health and scrubs, copy-job sweep
    monolithd job rsync ARGS...              run an rsync copy as a job with rsync's own progress
    monolithd storage-status MOUNT...        print what the storage observer sees, once
    monolithd sleep-policy                   observe when the machine would suspend
    monolithd probe-header ...               header LED mapping test (see tools/probe-header.sh)

## Monolith Remote

`monolithd remote [PORT]` serves the remote API on loopback only; put it behind
something that authenticates the network (the author uses Tailscale Serve), never on an
open port. It runs as its own unit from a separately installed copy of the binary, so
a lighting rebuild cannot disturb it: `install -m 0700 target/release/monolithd
~/.local/lib/monolith-remote/monolithd`, then restart the unit. `/status` includes a
lighting summary and the sleep policy's verdict.

## History

The project began in a private server-configuration repository and moved here on
2026-09-25. Commit messages from before the move cite hashes from that repository. The
design notes and decision log live in the author's private notes; the code comments
carry the reasoning that matters for each part.

## License

Copyright (C) 2026 Sebastián Jiménez

This program is free software: you can redistribute it and/or modify it under the terms
of the GNU General Public License as published by the Free Software Foundation, either
version 3 of the License, or (at your option) any later version. It is distributed in
the hope that it will be useful, but WITHOUT ANY WARRANTY; without even the implied
warranty of MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE. See [LICENSE](LICENSE)
for the full text.

Contributions are accepted under the same license.
