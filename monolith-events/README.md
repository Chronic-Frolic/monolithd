# Monolith Event Controller

Local-only appliance-state authority for White Monolith. It controls RGB through
a persistent loopback OpenRGB SDK client and cannot request power actions.

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

launch-scenes.toml is intentionally empty until a desired profile exists.
Create and save an OpenRGB profile, copy it to profiles/scenes/, then bind its
Steam shortcut App ID to a named scene in that file. Unbound launches remain
observation only.

## Gamescope Game Observer

monolith_gamescope_game_observer.py detects Steam reaper SteamLaunch AppId
processes, not Gamescope session presence. It reports launch lifetime through
controller status and changes neither RGB nor power. It covers Steam-launched
native, Proton, and Steam-added shortcuts; raw shortcut detection is not a
claim that the shortcut is a game.
