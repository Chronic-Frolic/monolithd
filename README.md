# monolithd

Lighting and machine-state daemon for White Monolith. QLC+ authors and renders every
look; `monolithd` selects registered QLC+ Functions from truthful state, carries QLC's
E1.31 frames to OpenRGB, supervises sleep and failure, and serves the narrow Monolith
Remote API. Design decisions and current status live in the vault note "Monolith Event
Controller - Design and Implementation Handoff"; `docs/runbook.md` covers operations.

## Layout

    src/        the Rust crate (one binary, many subcommands)
    config/     everything the binary reads at runtime (see below)
    qlcplus/    production QLC+ workspace and the owner-editable intake workspaces
    systemd/    user units, the lighting-stack supervisor and author-mode scripts
    python/     the read-only suspend-status tool and its test
    tools/      probe-header.sh and the intake template builder
    docs/       runbook

The binary finds `config/` through the source tree it was built from, so a moved
checkout needs a rebuild. Runtime state keeps its own names outside the repository:
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

## Commands

Build inside the `monolith-rust` toolbox with `cargo build --release`; run from
`target/release/`:

    monolithd validate-registry              check the registry and calibration against the QLC+ workspace
    monolithd workspace select NAME [--check] import an owner-authored intake workspace
    monolithd scene status                   gateway state, zone owners, calibration in force, output health
    monolithd scene start-set A B C          start several zone functions in phase
    monolithd scene progress ZONE N          select progress step N for a zone
    monolithd scene replace|start|stop NAME  see the gateway rules in the vault note
    monolithd scene rejoin NAME              return a zone to its ambient function, in phase with the running ones
    monolithd controller                     run the controller (lighting state and job leases)
    monolithd event status                   controller state: ambient set, zones, jobs, recent actions
    monolithd event job-start ID LABEL TOTAL announce a job (then job-progress, job-complete, job-fail)
    monolithd event pause | resume           stop or resume the controller acting on the plant
    monolithd calibrate --show               list the gains and what full white becomes
    monolithd calibrate ZONE R G B           set a zone gain (0.0 to 1.0), applied within a second
    monolithd steam-reporter                 report running Steam updates as controller jobs
    monolithd probe-header ...               header LED mapping test (see tools/probe-header.sh)

## Monolith Remote

`monolithd remote [PORT]` serves the machine's narrow remote API (suspend, reboot, power
off, Gaming and Desktop Mode, and the manual suspend block) behind a bearer token, on
loopback only; Tailscale Serve exposes it. It runs as its own unit,
`monolith-remote.service`, from a deliberately installed copy of the binary so a
lighting rebuild cannot disturb it: `install -m 0700 target/release/monolithd
~/.local/lib/monolith-remote/monolithd`, then restart the unit. `/status` includes a
lighting summary.

## History

The Python controller, watchdog, SDK bridge and palette renderer, their units, and the
OpenRGB profiles were retired and removed on 2026-09-25; recover them from Git history
if ever needed. Until 2026-09-25 this project lived in `server-config`, where its
earlier commit hashes remain.
