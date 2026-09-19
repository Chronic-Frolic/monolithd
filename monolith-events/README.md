# Monolith Event Controller

The Monolith Event Controller is the appliance-state authority for White Monolith.
It accepts truthful events from workloads and user controls, normalizes them into a
single state, and exposes that state to separate power-policy and RGB-rendering
adapters.

## Frozen boundaries

- Systemd/logind inhibitors remain the real safety authority for active workloads.
- The power adapter alone may request automatic suspend.
- The RGB adapter only renders state and can never initiate a power action.
- Profiles are whole-appliance presets (`off`, `gaming`, rollback); direct OpenRGB
  SDK control supplies operational signaling.
- No state is inferred as task progress from raw CPU/GPU load.

## State priority

`fault > warning > working > gaming > idle`

Working currently overrides Gaming so background activity remains visible during
gaming. This ordering will be revisited only after real use shows that server
workloads do not affect gaming performance.

## Event contract

```text
mode: idle | gaming | working | warning | fault
task.start(id, label, total?)
task.progress(id, completed, total)
task.complete(id)
task.fail(id, reason)
suspend_block.set(reason, expires_at)
suspend_block.clear()
fault.raise(id, severity, reason)
fault.clear(id)
```

Future implementations must expose a read-only JSON status report. Runtime
snapshots are reports, not the power-safety mechanism.

## RGB working-state manual render

`rgb_renderer.py` is the first direct-SDK renderer. It intentionally takes
explicit `0`-`8` levels and refuses to change LEDs without `--apply`; it does
not yet read system metrics, infer task progress, schedule itself, or make any
power decision.

The approved visual language is full brightness only:

- physical RAM left-to-right: CPU, GPU, and memory utilization as bottom-to-top white bars;
- physical RAM fourth: tracked task progress/state as a bottom-to-top green bar;
- ROG-eye logo: white general-status indicator; warnings and faults will
  replace white with their status color;
- the currently unmapped motherboard-header LEDs remain off.

Example manual test (levels are intentionally obvious and non-semantic):

```bash
~/.local/share/monolith-events/venv/bin/python ~/server-config/monolith-events/rgb_renderer.py \
  --apply --cpu 2 --gpu 4 --memory 6 --task 5
```

Leave an applied manual scene visible for physical inspection. Restore the
known-safe all-off profile after a rejected test or when no render is wanted.

## RGB all-RAM task-progress render

The separate task-progress scene uses all four physical RAM modules as one
32-segment progress bar. Each segment is white until completion and turns green
in order, bottom-to-top within a module and then left-to-right across modules.
It is reserved for workloads that report genuine progress; it must not turn
raw utilization into invented progress.

Manual test:
    ~/.local/share/monolith-events/venv/bin/python ~/server-config/monolith-events/rgb_renderer.py --apply --scene working-progress --task-progress 13

The ROG-eye remains white for normal status. Progress completion behavior
(brief all-green acknowledgement followed by state clear) belongs to the later
event-state layer, not this manual renderer.

## Semantic RGB palette

rgb-palette.toml is the sole color source for direct-SDK operational renders.
It requires two RGB-triplet roles: primary for normal/status baseline lighting
and secondary for utilization, progress, and task fill. The default palette is
white primary and green secondary. The renderer refuses to apply a scene if
this file is absent or invalid, rather than falling back to hardcoded colors.

In the working utilization scene, every RAM LED starts primary and the lower
zero-to-eight LEDs of each physical module turn secondary for CPU, GPU, memory,
or tracked-task level. The working-progress scene uses the identical primary to
secondary language across all 32 RAM LEDs. The ROG-eye uses primary for normal
status. Static OpenRGB profile fallbacks are intentionally independent of this
palette.
