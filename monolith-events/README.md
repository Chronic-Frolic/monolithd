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
