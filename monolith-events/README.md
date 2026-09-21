# Monolith Event Controller

Local-only appliance-state authority for White Monolith. It controls RGB through
a persistent loopback OpenRGB SDK client and cannot request power actions.

> **Status, 2026-09-21.** The Python controller and watchdog described in the
> older sections below are no longer loaded. Lighting runs as
> systemd/monolith-lighting-stack.service: the OpenRGB SDK server, QLC+ and
> monolithd e131-receiver in one private network namespace. Design decisions
> and current status are kept in the vault note "Monolith Event Controller -
> Design and Implementation Handoff"; the older sections are historical.

## Current lighting configuration

- scene-layout.toml: zone identity, the QLC+ and E1.31 listeners, boot Function.
- qlc-functions.toml: registry of QLC+ Function IDs as workspace API (semantic
  name, ID, owned zones, composability, progress step ranges) plus per-zone DMX
  geometry.
- led-calibration.toml: per-zone R/G/B gain applied at the very end of the
  output path. Scenes are authored in nominal color (white is 255, 255, 255);
  hardware color correction lives only here.

Commands, from monolithd/target/release/:

    monolithd validate-registry              check the registry and calibration against the QLC+ workspace
    monolithd scene status                   gateway state, zone owners, calibration in force, output health
    monolithd scene start-set A B C          start several zone functions in phase
    monolithd scene progress ZONE N          select progress step N for a zone
    monolithd scene replace|start|stop NAME  see the gateway rules in the vault note
    monolithd calibrate --show               list the gains and what full white becomes
    monolithd calibrate ZONE R G B           set a zone gain (0.0 to 1.0), applied within a second

## Priority and composition

fault > warning overlay > rgb-quiet > working > launch scene > idle

The controller renders a base scene, then overlays status where appropriate.

- Idle is full primary color across RAM and ROG eye.
- Working uses primary RAM baselines with bottom-to-top secondary telemetry fill;
  the ROG eye is secondary.
- A warning preserves the base scene and changes only the ROG eye to warning.
- RGB Quiet loads All Off.
- Fault is a synchronized red/primary ambulance animation. If its direct
  animation fails, the static all-red fault fallback is used. Controller or
  watchdog failure is the distinct magenta/black Controller Fault profile.
- Launch scenes will be versioned OpenRGB profiles selected by explicit
  shortcut bindings. They are lower priority than working.

Palette roles live only in rgb-palette.toml: primary, secondary, warning, and
fault are currently white, green, amber, and red.

## Controller and watchdog

monolith_event_controller.py owns a private Unix socket under XDG_RUNTIME_DIR.
monolith-eventctl provides status, health, utilization, progress, warning,
fault, quiet, reset, and mode commands. Normal direct rendering reuses one SDK
client, skips redundant mode transitions, and uses fast whole-controller
writes; physical testing confirmed crisp, flash-free transitions.

monolith_event_watchdog.py never starts the controller. It health-checks the
controller and SDK every 10 seconds while awake, applies Controller Fault on
failure, and restores saved state only after a healthy recovery. It also holds
a delay inhibitor, sets hardware Off before sleep, and gives controller
rendering 15 seconds after wake before faulting. Automatic suspension remains
disabled.

systemd/ contains the canonical user units.

## Launch-scene registry

launch-scenes.toml is intentionally empty until a desired scene exists. The
OpenRGB-profile launch scenes were removed (they belonged to the Effects-plugin
architecture, superseded by QLC+); launch scenes will name QLC+ scenes or
ambient sets from qlc-functions.toml once the controller exists. Unbound
launches remain observation only.

## Gamescope Game Observer

monolith_gamescope_game_observer.py detects Steam reaper SteamLaunch AppId
processes, not Gamescope session presence. It reports launch lifetime through
controller status and changes neither RGB nor power. It covers Steam-launched
native, Proton, and Steam-added shortcuts; raw shortcut detection is not a
claim that the shortcut is a game.
