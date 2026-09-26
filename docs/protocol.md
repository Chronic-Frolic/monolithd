# The controller protocol

`monolithd controller` owns what the lights should show. Anything else on the machine
tells it what is happening through one Unix socket: a job started, moved, finished or
failed; something needs attention; the room wants quiet. The built-in reporters (Steam
downloads, storage health, `monolithd job rsync`) are ordinary clients of this socket,
and so is anything you write. A reporter needs no Rust and no changes to the daemon.

## Transport

- Socket: `$XDG_RUNTIME_DIR/monolith-events/controller.sock`, a Unix stream socket.
- Only processes running as the socket's owner may connect; any other uid (root
  included) is disconnected.
- JSON lines: each request is one JSON object on one line, ending in `\n`, at most
  4096 bytes. Each request gets exactly one reply line. A connection may carry several
  requests and closes after 30 s without one.
- Unknown fields and unknown ops are rejected, so a typo fails loudly.

A reply is either

    {"ok": true, ...}

or

    {"ok": false, "code": "unknown_job", "error": "unknown job steam:292030"}

| `code` | Meaning |
|---|---|
| `bad_request` | Malformed JSON, a missing or unknown field, a zero total, an unknown zone, or a job that is already in its completion hold |
| `unknown_job` | The controller has no job with that ID (it may have restarted) |
| `unknown_fault` | `fault.clear` for an ID that is not raised |
| `unknown_set` | `ambient.select` with a set that is not registered |
| `unknown_pattern` | `job.start` with a progress pattern that is not registered |
| `request_too_large` | The line was over 4096 bytes or had no newline |

## Operations

### Jobs

| Request | Effect |
|---|---|
| `{"op":"job.start","id":ID,"label":TEXT,"total":N}` | Announce a job. Optional: `"priority":INT` (default 0) and `"pattern":NAME`, a registered progress family to draw with instead of the zone's default. Sending it again for a live job updates the label, total, priority and pattern, so re-announcing is safe. |
| `{"op":"job.progress","id":ID,"completed":N}` | Report progress. Optional `"total":N` changes the total. `completed` is clamped to the total. Ignored during the completion hold. |
| `{"op":"job.complete","id":ID}` | The job finished: its bar shows 100% for `complete_hold_seconds` (15 s by default), then the zone returns to ambient. A job that never got a zone is simply dropped. |
| `{"op":"job.fail","id":ID,"reason":TEXT}` | The job failed: its zone is released at once and the failure is kept in `status` (the last 8). |

Zones that can show progress are listed in `config/controller.toml` as `progress_zones`,
in allocation order. A job takes the first free one; when none is free it waits. When a
zone frees up, the waiting job with the highest `priority` takes it (the earliest on a
tie). A job holding a zone is never displaced.

While any job is active, `monolithd watchdog` holds a block-mode sleep inhibitor, so the
machine does not suspend mid-job. That hold is capped at 6 hours continuous, so a
reporter that dies without ending its job cannot keep the machine awake forever.

### Warnings and faults

| Request | Effect |
|---|---|
| `{"op":"fault.raise","id":ID,"severity":"warning","reason":TEXT}` | Something needs attention. Shown on `warning_zones` from `controller.toml` (the ROG eye by default). |
| `{"op":"fault.raise","id":ID,"severity":"fault","reason":TEXT}` | Something is wrong. Shown on every zone. |
| `{"op":"fault.clear","id":ID}` | Clear it. |

Optional `"zones":["ram","strip"]` picks the zones explicitly; each must have an asset
for that severity registered in `qlc-functions.toml`. Raising an ID that is already
raised replaces it. Precedence on a zone is fault, then warning, then quiet, then the
automatic "a job is running" indicator, then progress or ambient.

### Everything else

| Request | Effect |
|---|---|
| `{"op":"status"}` | The whole state: ambient set, zones and their owners, jobs, recent failures, active faults, quiet, recent actions. |
| `{"op":"ambient.select","set":NAME}` | Switch the ambient set. |
| `{"op":"quiet.set","reason":TEXT}` / `{"op":"quiet.clear"}` | Switch every zone to its authored quiet look (for example for a film), and back. |
| `{"op":"pause"}` / `{"op":"resume"}` | Stop or resume the controller acting on the lights. The watchdog uses these around suspend; leave them alone otherwise. |

## From the shell

`monolithd event` sends one request and prints the reply:

    monolithd event job-start backup:photos "Photo backup" 100
    monolithd event job-progress backup:photos 42
    monolithd event job-complete backup:photos
    monolithd event job-fail backup:photos disk is full
    monolithd event fault-raise ups:battery warning UPS on battery
    monolithd event fault-clear ups:battery
    monolithd event status

It exits nonzero on a refusal, printing the code and error.

## From anything else

Any language that can open a Unix socket will do. In Python:

```python
import json, os, socket

def call(request):
    path = os.path.join(os.environ["XDG_RUNTIME_DIR"], "monolith-events", "controller.sock")
    with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as sock:
        sock.connect(path)
        sock.sendall((json.dumps(request) + "\n").encode())
        return json.loads(sock.makefile().readline())

call({"op": "job.start", "id": "render:scene12", "label": "Render scene 12", "total": 240})
```

## Rules for a good reporter

The controller keeps its state in memory only. These rules keep the lights truthful
across restarts and crashes; `src/reporter.rs` implements them for the built-in
reporters.

1. **Truthful totals.** The bar is `completed / total`. Report real units (bytes, files,
   frames, percent) from the service's own API, never a guess or a timer.
2. **Stable IDs** of the form `service:item`, for example `steam:292030` or
   `rsync:<pid>`. The same work always gets the same ID.
3. **Re-announce on `unknown_job`.** It means the controller restarted. Send `job.start`
   again, then carry on.
4. **End every job exactly once.** Send `job.complete` or `job.fail`. If the controller
   is unreachable, keep the ending and retry it; never drop it.
5. **Reassert warnings periodically.** A restart forgets them too. Raising an ID again
   is harmless.
6. **Faults are for real harm** (data at risk, a failed disk, a service down for good).
   Everything else is a warning.
7. **Fail loudly.** When the reporter cannot see its service, raise a warning instead of
   showing nothing: no bar looks the same as nothing to report.
