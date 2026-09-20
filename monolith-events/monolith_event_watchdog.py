#!/usr/bin/env python3
"""Independent health and suspend lifecycle watchdog for the Monolith Event Controller."""

from __future__ import annotations

import argparse
import subprocess
import threading
import time

from monolith_event_controller import OPENRGB, RENDERER, VENV_PYTHON, profile_path, send

HEALTH_INTERVAL_SECONDS = 10
RESUME_GRACE_SECONDS = 15


class DelayInhibitor:
    def __init__(self) -> None:
        self.process: subprocess.Popen[str] | None = None

    def acquire(self) -> None:
        if self.process and self.process.poll() is None:
            return
        command = [
            "systemd-inhibit", "--what=sleep", "--mode=delay",
            "--who=Monolith-Event-Watchdog", "--why=RGB suspend handoff",
            "/usr/bin/sleep", "infinity",
        ]
        self.process = subprocess.Popen(command, text=True, stdout=subprocess.DEVNULL, stderr=subprocess.PIPE)
        time.sleep(0.2)
        if self.process.poll() is not None:
            stderr = self.process.stderr.read() if self.process.stderr else ""
            raise RuntimeError(stderr.strip() or "could not acquire sleep delay inhibitor")

    def release(self) -> None:
        if self.process and self.process.poll() is None:
            self.process.terminate()
            try:
                self.process.wait(timeout=2)
            except subprocess.TimeoutExpired:
                self.process.kill()
                self.process.wait(timeout=2)
        self.process = None


def apply_fault() -> None:
    result = subprocess.run([str(OPENRGB), "--profile", str(profile_path("controller_failure"))], text=True, capture_output=True, timeout=20)
    if result.returncode != 0:
        raise RuntimeError((result.stderr or result.stdout).strip() or "controller fault profile failed")


def apply_off() -> None:
    result = subprocess.run([str(VENV_PYTHON), str(RENDERER), "--apply", "--scene", "off"], text=True, capture_output=True, timeout=4)
    if result.returncode != 0:
        raise RuntimeError((result.stderr or result.stdout).strip() or "hardware Off handoff failed")


class Watchdog:
    def __init__(self) -> None:
        self.lock = threading.RLock()
        self.delay = DelayInhibitor()
        self.suspended = False
        self.fault_active = False
        self.stop = threading.Event()

    def fault(self, reason: str) -> None:
        if not self.fault_active:
            apply_fault()
            self.fault_active = True
            print(f"Monolith Event Watchdog: Controller Fault applied: {reason}", flush=True)

    def check_health(self) -> None:
        response = send({"action": "health"}, timeout=3)
        if not response.get("ok"):
            raise RuntimeError(response.get("error", "controller health request failed"))
        if response.get("state", {}).get("last_error"):
            raise RuntimeError(response["state"]["last_error"])
        if self.fault_active:
            restored = send({"action": "render"}, timeout=10)
            if not restored.get("ok"):
                raise RuntimeError(restored.get("error", "controller recovery render failed"))
            self.fault_active = False
            print("Monolith Event Watchdog: controller recovered and state restored", flush=True)

    def health_loop(self) -> None:
        while not self.stop.wait(HEALTH_INTERVAL_SECONDS):
            with self.lock:
                if self.suspended:
                    continue
                try:
                    self.check_health()
                except Exception as error:
                    try:
                        self.fault(str(error))
                    except Exception as fault_error:
                        print(f"Monolith Event Watchdog: fault render failed: {fault_error}", flush=True)

    def prepare_sleep(self) -> None:
        with self.lock:
            self.suspended = True
            try:
                apply_off()
                print("Monolith Event Watchdog: hardware Off applied before sleep", flush=True)
            except Exception as error:
                self.fault(f"pre-sleep handoff failed: {error}")
            finally:
                self.delay.release()

    def resume(self) -> None:
        with self.lock:
            self.delay.acquire()
            self.suspended = False
            deadline = time.monotonic() + RESUME_GRACE_SECONDS
            last_error = "controller did not respond"
            while time.monotonic() < deadline:
                try:
                    response = send({"action": "render"}, timeout=3)
                    if response.get("ok"):
                        self.fault_active = False
                        print("Monolith Event Watchdog: controller recovered after resume", flush=True)
                        return
                    last_error = response.get("error", last_error)
                except Exception as error:
                    last_error = str(error)
                time.sleep(1)
            self.fault(f"controller did not recover after resume: {last_error}")

    def monitor_sleep(self) -> None:
        command = ["gdbus", "monitor", "--system", "--dest", "org.freedesktop.login1", "--object-path", "/org/freedesktop/login1"]
        self.delay.acquire()
        while not self.stop.is_set():
            process = subprocess.Popen(command, text=True, stdout=subprocess.PIPE, stderr=subprocess.STDOUT)
            assert process.stdout is not None
            for line in process.stdout:
                if "PrepareForSleep" not in line:
                    continue
                if "true" in line.lower():
                    self.prepare_sleep()
                elif "false" in line.lower():
                    self.resume()
            process.wait()
            if not self.stop.wait(2):
                continue

    def run(self) -> None:
        thread = threading.Thread(target=self.health_loop, name="monolith-health", daemon=True)
        thread.start()
        try:
            self.monitor_sleep()
        finally:
            self.stop.set()
            self.delay.release()


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--fault", action="store_true", help="apply the independent Controller Fault profile once")
    args = parser.parse_args()
    if args.fault:
        apply_fault()
    else:
        Watchdog().run()


if __name__ == "__main__":
    main()
