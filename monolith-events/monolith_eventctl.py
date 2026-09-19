#!/home/chronic_frolic/.local/share/monolith-events/venv/bin/python
"""Manual local client for the Monolith Event Controller."""

from __future__ import annotations

import argparse
import json
import sys

from monolith_event_controller import send


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    subparsers = parser.add_subparsers(dest="command", required=True)
    subparsers.add_parser("status")
    subparsers.add_parser("health")
    subparsers.add_parser("render")
    subparsers.add_parser("reset")
    mode = subparsers.add_parser("mode")
    mode.add_argument("mode", choices=("idle", "gaming"))
    utilization = subparsers.add_parser("utilization")
    for name in ("cpu", "gpu", "memory", "task"):
        utilization.add_argument(f"--{name}", type=int, required=True, choices=range(9))
    progress = subparsers.add_parser("progress")
    progress.add_argument("completed", type=int, choices=range(33))
    for name in ("warning", "fault"):
        command = subparsers.add_parser(name)
        command.add_argument("action", choices=("set", "clear"))
        command.add_argument("label", nargs="?", default="manual event")
    quiet = subparsers.add_parser("quiet")
    quiet.add_argument("action", choices=("on", "off"))
    args = parser.parse_args()

    if args.command in ("status", "health", "render", "reset"):
        request = {"action": args.command}
    elif args.command == "mode":
        request = {"action": "mode", "mode": args.mode}
    elif args.command == "utilization":
        request = {"action": "utilization", **{name: getattr(args, name) for name in ("cpu", "gpu", "memory", "task")}}
    elif args.command == "progress":
        request = {"action": "progress", "completed": args.completed}
    elif args.command in ("warning", "fault"):
        request = {"action": args.command, "active": args.action == "set", "label": args.label}
    else:
        request = {"action": "quiet", "active": args.action == "on"}

    response = send(request)
    print(json.dumps(response, indent=2, sort_keys=True))
    raise SystemExit(0 if response.get("ok") else 1)


if __name__ == "__main__":
    main()
