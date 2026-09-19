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

- physical RAM left-to-right: CPU, GPU, and memory utilization as white bars;
- physical RAM fourth: tracked task progress/state as a green bar;
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
