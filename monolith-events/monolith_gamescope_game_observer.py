#!/usr/bin/env python3
"""Observe actual Steam game launches in a Gamescope session without changing state."""

from __future__ import annotations

import argparse
import json
import os
from pathlib import Path
import re
import subprocess
import tempfile
import time

RUNTIME = Path(os.environ.get("XDG_RUNTIME_DIR", f"/run/user/{os.getuid()}")) / "monolith-events"
STATE_FILE = RUNTIME / "gamescope-game-observer.json"
APP_ID = re.compile(r"SteamLaunch AppId=(\d+)")


def gamescope_session_active() -> bool:
    result = subprocess.run(
        ["systemctl", "--user", "is-active", "gamescope-session-plus@ogui-steam.service"],
        text=True,
        capture_output=True,
        timeout=2,
    )
    return result.returncode == 0


def steam_launches() -> list[dict[str, object]]:
    launches: list[dict[str, object]] = []
    for entry in Path("/proc").iterdir():
        if not entry.name.isdigit():
            continue
        try:
            if (entry / "comm").read_text().strip() != "reaper":
                continue
            command = (entry / "cmdline").read_bytes().replace(b"\0", b" ").decode(errors="replace")
        except OSError:
            continue
        match = APP_ID.search(command)
        if match:
            launches.append({"app_id": int(match.group(1)), "pid": int(entry.name)})
    return sorted(launches, key=lambda item: (item["app_id"], item["pid"]))


def write_state(state: dict[str, object]) -> None:
    RUNTIME.mkdir(mode=0o700, parents=True, exist_ok=True)
    with tempfile.NamedTemporaryFile("w", dir=RUNTIME, prefix=".gamescope-", delete=False) as output:
        json.dump(state, output, sort_keys=True)
        output.write("\n")
        temporary = Path(output.name)
    temporary.chmod(0o600)
    temporary.replace(STATE_FILE)


def current_state() -> dict[str, object]:
    launches = steam_launches()
    return {
        "schema": 1,
        "checked_at": int(time.time()),
        "gamescope_session_active": gamescope_session_active(),
        "active_game_count": len(launches),
        "steam_launches": launches,
        "signal": "Steam reaper SteamLaunch AppId",
        "coverage": "Steam-launched native, Proton, and Steam-added shortcut games only",
        "mode": "observe-only",
    }


def serve() -> None:
    previous: set[tuple[int, int]] = set()
    while True:
        state = current_state()
        current = {(item["app_id"], item["pid"]) for item in state["steam_launches"]}
        for app_id, pid in sorted(current - previous):
            print(f"Gamescope Game Observer: launch app_id={app_id} reaper_pid={pid}", flush=True)
        for app_id, pid in sorted(previous - current):
            print(f"Gamescope Game Observer: exit app_id={app_id} reaper_pid={pid}", flush=True)
        previous = current
        write_state(state)
        time.sleep(2)


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("command", choices=("serve", "status"), default="status", nargs="?")
    args = parser.parse_args()
    if args.command == "serve":
        serve()
    try:
        print(STATE_FILE.read_text(), end="")
    except OSError:
        raise SystemExit(f"observer state unavailable: {STATE_FILE}")


if __name__ == "__main__":
    main()
