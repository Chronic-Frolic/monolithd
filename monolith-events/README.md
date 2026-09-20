# Monolith Event Controller

The Monolith Event Controller is White Monolith's local appliance-state
authority. It normalizes explicit local events and drives RGB through a
loopback-only OpenRGB SDK. It has no network listener and cannot request
suspension, reboot, shutdown, or arbitrary commands.

## Boundaries

- Systemd and logind inhibitors remain the safety authority for active work.
- Automatic suspension remains disabled.
- The controller renders state only; a later power-policy adapter may request
  suspension after observation proves trustworthy.
- No task progress is inferred from CPU, GPU, or memory utilization.
- Profiles are whole-appliance fallbacks. Direct SDK rendering is for
  operational signals.

## State priority

fault > warning > rgb-quiet > working > gaming > idle

Working deliberately overrides Gaming while server work may affect play. Gaming
currently falls back to All Off until its aesthetic profile exists.

## Palette and verified renders

rgb-palette.toml is the sole color source for direct operational renders. Its
roles are primary, secondary, warning, and fault; current values are white,
green, amber, and red. Static fallback profiles are intentionally independent.

The verified working utilization view uses primary as every RAM LED baseline,
then fills the lower LEDs secondary, bottom-to-top. Physical RAM left-to-right
represents CPU, GPU, memory, and tracked-task level. The verified
working-progress view is one 32-segment primary-to-secondary task bar across
all RAM, bottom-to-top within a module and left-to-right across modules. The
ROG eye is primary for normal status.

Warning turns RAM hardware Off and uses the ROG eye in the warning color. Fault
uses the independent Controller Fault profile: magenta/black RAM checkerboard
and solid-magenta ROG eye. RGB Quiet, idle, and the current gaming fallback
load All Off.

rgb_renderer.py is both a guarded manual CLI and the controller's reusable
renderer. The controller keeps one persistent SDK client, skips redundant
Direct-mode changes, and sends fast whole-controller writes. Physical testing
confirmed crisp transitions with no transient colors.

## Local controller

monolith_event_controller.py runs as monolith-event-controller.service. Its
runtime state and Unix socket are private under XDG_RUNTIME_DIR and disappear
on reboot. monolith-eventctl is its local manual client:

    monolith-eventctl status
    monolith-eventctl health
    monolith-eventctl utilization --cpu 2 --gpu 4 --memory 6 --task 5
    monolith-eventctl progress 13
    monolith-eventctl warning set "manual warning"
    monolith-eventctl fault set "manual fault"
    monolith-eventctl quiet on

Health is read-only: it checks that both the controller socket and its
persistent OpenRGB SDK connection respond without rewriting LEDs.

## Independent Event Watchdog

monolith_event_watchdog.py runs separately as monolith-event-watchdog.service.
It never starts or restarts the controller. While awake it performs the
read-only health check every 10 seconds. A missing, hung, or errored controller
applies Controller Fault. Once a healthy controller returns, the watchdog asks
it to restore its saved state.

The watchdog also owns RGB suspend lifecycle. It holds a logind delay
inhibitor, sets all controllers to hardware Off before sleep, releases the
delay, then gives the controller 15 seconds to render after wake. Failure to
recover applies Controller Fault. Sleep handoff, normal wake recovery, awake
controller-loss faulting, fault persistence, and automatic recovery are
physically verified.

systemd/ contains the canonical source for the controller, watchdog, and
one-shot failsafe units. The failsafe applies Controller Fault if either daemon
itself exits unexpectedly.

## Observe-only Gamescope Game Adapter

monolith_gamescope_game_observer.py is a separate read-only adapter. It observes
Steam reaper processes carrying SteamLaunch AppId, not the Gamescope session
itself. Its snapshot is included in controller status and records app ID, reaper
PID, active count, and Gamescope session context.

Physical validation used The Witcher 3 (Steam App ID 292030, Proton): launch was
observed while Gamescope was active, then exit reduced active_game_count to zero
while Gamescope remained active. This adapter does not render RGB or change
controller state. Coverage is Steam-launched native, Proton, and Steam-added
shortcut games; direct non-Steam launchers remain a separate future adapter.
