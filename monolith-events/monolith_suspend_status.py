#!/usr/bin/env python3
"""Read-only suspend-policy observer for White Monolith."""

from __future__ import annotations

import argparse
import json
import re
import subprocess
import time
from dataclasses import asdict, dataclass
from typing import Any


@dataclass
class Inhibitor:
    who: str
    what: str
    why: str
    mode: str


def run(*args: str) -> str:
    return subprocess.run(args, check=True, text=True, stdout=subprocess.PIPE).stdout


def property_value(output: str, name: str) -> str:
    prefix = f"{name}="
    for line in output.splitlines():
        if line.startswith(prefix):
            return line.removeprefix(prefix)
    return ""


def active_seat0_session() -> str | None:
    output = run("loginctl", "show-seat", "seat0", "-p", "ActiveSession")
    value = property_value(output, "ActiveSession")
    return value or None


def seat_idle_status(session_id: str | None) -> dict[str, Any]:
    if session_id is None:
        return {"session": None, "idle": None, "idle_seconds": None}
    output = run(
        "loginctl", "show-session", session_id,
        "-p", "IdleHint", "-p", "IdleSinceHintMonotonic",
        "-p", "Type", "-p", "Remote", "-p", "State",
    )
    idle = property_value(output, "IdleHint") == "yes"
    since_usec = property_value(output, "IdleSinceHintMonotonic")
    idle_seconds: float | None = None
    if idle and since_usec.isdigit():
        idle_seconds = max(0.0, time.clock_gettime(time.CLOCK_MONOTONIC) - int(since_usec) / 1_000_000)
    return {
        "session": session_id,
        "idle": idle,
        "idle_seconds": round(idle_seconds, 1) if idle_seconds is not None else None,
        "type": property_value(output, "Type"),
        "remote": property_value(output, "Remote"),
        "state": property_value(output, "State"),
    }


def inhibitors() -> list[Inhibitor]:
    output = run("systemd-inhibit", "--list", "--no-legend", "--no-pager")
    found: list[Inhibitor] = []
    pattern = re.compile(r"^\s*(\S+)\s+\d+\s+\S+\s+\d+\s+\S+\s+(\S+)\s+(.*?)\s+(block|delay)\s*$")
    for line in output.splitlines():
        match = pattern.match(line)
        if match:
            found.append(Inhibitor(who=match.group(1), what=match.group(2), why=match.group(3), mode=match.group(4)))
    return found


def manual_block_status() -> dict[str, Any]:
    output = run("systemctl", "--user", "show", "monolith-suspend-block.service", "-p", "ActiveState", "-p", "ActiveEnterTimestampMonotonic")
    active = property_value(output, "ActiveState") == "active"
    started_usec = property_value(output, "ActiveEnterTimestampMonotonic")
    expires_in_seconds: int | None = None
    if active and started_usec.isdigit():
        elapsed = time.clock_gettime(time.CLOCK_MONOTONIC) - int(started_usec) / 1_000_000
        expires_in_seconds = max(0, round(12 * 60 * 60 - elapsed))
    return {"active": active, "expires_in_seconds": expires_in_seconds}


def report() -> dict[str, Any]:
    session = active_seat0_session()
    seat = seat_idle_status(session)
    locks = inhibitors()
    manual_block = manual_block_status()
    sleep_blocks = [lock for lock in locks if lock.mode == "block" and "sleep" in lock.what.split(":")]
    reasons: list[str] = []
    if not seat["idle"]:
        reasons.append("physical seat0 session is not idle")
    if sleep_blocks:
        reasons.append("blocking sleep inhibitor is active")
    if manual_block["active"]:
        reasons.append("manual remote suspend block is active")
    else:
        reasons.append("manual suspend block adapter is available but inactive")
    reasons.append("gaming/streaming safeguard adapter is not implemented")
    reasons.append("observe-only mode never requests suspend")
    return {
        "schema_version": 1,
        "observe_only": True,
        "auto_suspend_eligible": False,
        "seat0": seat,
        "manual_suspend_block": manual_block,
        "inhibitors": [asdict(lock) for lock in locks],
        "sleep_blockers": [asdict(lock) for lock in sleep_blocks],
        "reasons": reasons,
    }


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--json", action="store_true", help="emit JSON only")
    args = parser.parse_args()
    data = report()
    if args.json:
        print(json.dumps(data, indent=2, sort_keys=True))
        return
    print("Monolith suspend status (observe-only)")
    print(json.dumps(data, indent=2, sort_keys=True))


if __name__ == "__main__":
    main()
