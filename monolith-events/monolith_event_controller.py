#!/usr/bin/env python3
"""Local-only state authority for the Monolith Event Controller."""

from __future__ import annotations

import argparse
import asyncio
import json
import os
from pathlib import Path
import subprocess
import tempfile
import threading
import time
from typing import Any

from openrgb import OpenRGBClient
from rgb_renderer import load_palette, overlay_warning, render_fault_phase, render_idle, render_working, render_working_progress

ROOT = Path(__file__).resolve().parent
RUNTIME = Path(os.environ.get("XDG_RUNTIME_DIR", f"/run/user/{os.getuid()}")) / "monolith-events"
SOCKET = RUNTIME / "controller.sock"
STATE_FILE = RUNTIME / "state.json"
GAMESCOPE_OBSERVER_FILE = RUNTIME / "gamescope-game-observer.json"
VENV_PYTHON = Path.home() / ".local/share/monolith-events/venv/bin/python"
RENDERER = ROOT / "rgb_renderer.py"
OPENRGB = Path.home() / "AppImages/openrgb.appimage"
PROFILES = ROOT / "profiles"

DEFAULT_STATE: dict[str, Any] = {
    "schema": 1,
    "base_mode": "idle",
    "working_view": "utilization",
    "utilization": {"cpu": 0, "gpu": 0, "memory": 0, "task": 0},
    "task_progress": 0,
    "warning": None,
    "fault": None,
    "rgb_quiet": False,
    "fault_fallback_active": False,
    "last_error": None,
    "rendered_state": None,
}


def runtime_state() -> dict[str, Any]:
    RUNTIME.mkdir(mode=0o700, parents=True, exist_ok=True)
    try:
        loaded = json.loads(STATE_FILE.read_text())
        if loaded.get("schema") != 1:
            raise ValueError("unsupported state schema")
        state = DEFAULT_STATE | loaded
        state["utilization"] = DEFAULT_STATE["utilization"] | loaded.get("utilization", {})
        return state
    except (OSError, ValueError, json.JSONDecodeError):
        return json.loads(json.dumps(DEFAULT_STATE))


def write_state(state: dict[str, Any]) -> None:
    RUNTIME.mkdir(mode=0o700, parents=True, exist_ok=True)
    with tempfile.NamedTemporaryFile("w", dir=RUNTIME, prefix=".state-", delete=False) as output:
        json.dump(state, output, sort_keys=True)
        output.write("\n")
        temporary = Path(output.name)
    temporary.chmod(0o600)
    temporary.replace(STATE_FILE)


def adapter_status() -> dict[str, Any]:
    try:
        return json.loads(GAMESCOPE_OBSERVER_FILE.read_text())
    except (OSError, json.JSONDecodeError) as error:
        return {"available": False, "error": str(error)}


def resolved_state(state: dict[str, Any]) -> str:
    if state["fault"]:
        return "fault"
    if state["warning"]:
        return "warning"
    if state["rgb_quiet"]:
        return "rgb-quiet"
    return state["base_mode"]


def run(command: list[str], timeout: int = 15) -> str:
    result = subprocess.run(command, text=True, capture_output=True, timeout=timeout)
    if result.returncode != 0:
        details = (result.stderr or result.stdout).strip()
        raise RuntimeError(details or f"command failed with exit {result.returncode}")
    return result.stdout.strip()


def level(value: Any, name: str, maximum: int) -> int:
    if type(value) is not int or not 0 <= value <= maximum:
        raise ValueError(f"{name} must be an integer from 0 through {maximum}")
    return value


def update(state: dict[str, Any], request: dict[str, Any]) -> bool:
    action = request.get("action")
    if action in ("status", "health"):
        return False
    if action == "render":
        return True
    if action == "reset":
        state.clear()
        state.update(json.loads(json.dumps(DEFAULT_STATE)))
        return True
    if action == "mode":
        mode = request.get("mode")
        if mode not in ("idle", "gaming"):
            raise ValueError("mode must be idle or gaming")
        state["base_mode"] = mode
        return True
    if action == "utilization":
        state["base_mode"] = "working"
        state["working_view"] = "utilization"
        state["utilization"] = {name: level(request.get(name, 0), name, 8) for name in ("cpu", "gpu", "memory", "task")}
        return True
    if action == "progress":
        state["base_mode"] = "working"
        state["working_view"] = "progress"
        state["task_progress"] = level(request.get("completed", 0), "completed", 32)
        return True
    if action in ("warning", "fault"):
        active = request.get("active")
        if type(active) is not bool:
            raise ValueError(f"{action}.active must be boolean")
        state[action] = request.get("label", "manual event") if active else None
        return True
    if action == "quiet":
        active = request.get("active")
        if type(active) is not bool:
            raise ValueError("quiet.active must be boolean")
        state["rgb_quiet"] = active
        return True
    raise ValueError(f"unknown action: {action}")


class Controller:
    def __init__(self) -> None:
        self.state = runtime_state()
        self.client: OpenRGBClient | None = None
        self.lock = threading.RLock()
        self.fault_stop = threading.Event()
        self.fault_thread: threading.Thread | None = None

    def sdk_client(self) -> OpenRGBClient:
        if self.client is None:
            client = OpenRGBClient("127.0.0.1", 6742, "Monolith Event Controller")
            if len(client.devices) != 5:
                raise RuntimeError(f"expected 5 mapped controllers, found {len(client.devices)}")
            self.client = client
        return self.client

    def apply_profile(self, name: str) -> None:
        run([str(OPENRGB), "--profile", str(PROFILES / name)], timeout=20)
        self.client = None

    def direct(self, renderer) -> None:
        palette = load_palette()
        try:
            renderer(self.sdk_client(), palette)
        except Exception:
            self.client = None
            renderer(self.sdk_client(), palette)

    def stop_fault_animation(self) -> None:
        self.fault_stop.set()
        thread = self.fault_thread
        if thread and thread.is_alive() and thread is not threading.current_thread():
            thread.join(timeout=1)
        self.fault_thread = None
        self.fault_stop.clear()

    def fault_loop(self) -> None:
        phase = 0
        while not self.fault_stop.is_set():
            try:
                with self.lock:
                    if not self.state["fault"]:
                        return
                    self.direct(lambda client, palette: render_fault_phase(client, palette, phase))
                    self.state["fault_fallback_active"] = False
                    write_state(self.state)
            except Exception:
                try:
                    with self.lock:
                        self.apply_profile("fault-fallback.orp")
                        self.state["fault_fallback_active"] = True
                        write_state(self.state)
                except Exception:
                    pass
                return
            phase = 1 - phase
            self.fault_stop.wait(0.5)

    def start_fault_animation(self) -> None:
        if self.fault_thread and self.fault_thread.is_alive():
            return
        self.fault_stop.clear()
        self.fault_thread = threading.Thread(target=self.fault_loop, name="monolith-fault-animation", daemon=True)
        self.fault_thread.start()

    def render_base(self) -> None:
        if self.state["rgb_quiet"]:
            self.apply_profile("all-off.orp")
        elif self.state["base_mode"] == "working":
            if self.state["working_view"] == "progress":
                self.direct(lambda client, palette: render_working_progress(client, palette, self.state["task_progress"]))
            else:
                values = self.state["utilization"]
                self.direct(lambda client, palette: render_working(client, palette, values["cpu"], values["gpu"], values["memory"], values["task"]))
        else:
            self.direct(render_idle)

    def render(self) -> None:
        if self.state["fault"]:
            self.start_fault_animation()
            self.state["rendered_state"] = "fault"
            self.state["last_error"] = None
            return
        self.stop_fault_animation()
        self.state["fault_fallback_active"] = False
        self.render_base()
        if self.state["warning"]:
            self.direct(overlay_warning)
        self.state["rendered_state"] = resolved_state(self.state)
        self.state["last_error"] = None

    def health(self) -> None:
        self.sdk_client().update()
        self.state["last_healthy_at"] = int(time.time())

    def response(self, ok: bool, error: str | None = None) -> dict[str, Any]:
        result: dict[str, Any] = {"ok": ok, "active_state": resolved_state(self.state), "state": self.state, "adapters": {"gamescope_game_observer": adapter_status()}}
        if error:
            result["error"] = error
        return result

    def handle(self, request: dict[str, Any]) -> dict[str, Any]:
        with self.lock:
            try:
                should_render = update(self.state, request)
                if request.get("action") == "health":
                    self.health()
                if should_render:
                    self.render()
                write_state(self.state)
                return self.response(True)
            except Exception as error:
                self.state["last_error"] = str(error)
                write_state(self.state)
                return self.response(False, str(error))


async def serve() -> None:
    RUNTIME.mkdir(mode=0o700, parents=True, exist_ok=True)
    if SOCKET.exists():
        SOCKET.unlink()
    controller = Controller()
    initial = controller.handle({"action": "render"})
    if not initial["ok"]:
        raise RuntimeError(f"initial render failed: {initial['error']}")

    async def client(reader: asyncio.StreamReader, writer: asyncio.StreamWriter) -> None:
        try:
            response = controller.handle(json.loads((await reader.readline()).decode()))
        except Exception as error:
            response = {"ok": False, "error": str(error)}
        writer.write((json.dumps(response, sort_keys=True) + "\n").encode())
        await writer.drain()
        writer.close()
        await writer.wait_closed()

    server = await asyncio.start_unix_server(client, path=str(SOCKET))
    SOCKET.chmod(0o600)
    async with server:
        await server.serve_forever()


def send(request: dict[str, Any], timeout: int = 20) -> dict[str, Any]:
    if not SOCKET.exists():
        raise RuntimeError(f"controller socket unavailable: {SOCKET}")
    import socket
    with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as client:
        client.settimeout(timeout)
        client.connect(str(SOCKET))
        client.sendall((json.dumps(request) + "\n").encode())
        response = b""
        while not response.endswith(b"\n"):
            part = client.recv(65536)
            if not part:
                break
            response += part
    return json.loads(response)


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("operation", choices=("serve", "send"))
    parser.add_argument("--request")
    args = parser.parse_args()
    if args.operation == "serve":
        asyncio.run(serve())
    else:
        if not args.request:
            parser.error("--request is required for send")
        print(json.dumps(send(json.loads(args.request)), indent=2, sort_keys=True))


if __name__ == "__main__":
    main()
