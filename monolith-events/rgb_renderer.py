#!/usr/bin/env python3
"""Reusable direct-SDK renderer and guarded manual renderer CLI."""

from __future__ import annotations

import argparse
from dataclasses import dataclass
from pathlib import Path
import tomllib

from openrgb import OpenRGBClient
from openrgb.utils import RGBColor

PHYSICAL_RAM_DEVICE_ORDER = (1, 3, 0, 2)
BOARD_DEVICE = 4
ROG_EYE_ZONE = 0
ROG_EYE_LED_COUNT = 3
RAM_LEDS_PER_MODULE = 8
DEFAULT_PALETTE_PATH = Path(__file__).with_name("rgb-palette.toml")


@dataclass(frozen=True)
class Palette:
    off: RGBColor
    primary: RGBColor
    secondary: RGBColor
    warning: RGBColor
    fault: RGBColor
    controller_failure: RGBColor


def load_color(name: str, raw_color: object) -> RGBColor:
    if not isinstance(raw_color, list) or len(raw_color) != 3:
        raise ValueError(f"colors.{name} must be a three-item RGB list")
    if any(type(value) is not int or not 0 <= value <= 255 for value in raw_color):
        raise ValueError(f"colors.{name} values must be integers from 0 through 255")
    return RGBColor(*raw_color)


def load_palette(path: Path = DEFAULT_PALETTE_PATH) -> Palette:
    try:
        with path.open("rb") as palette_file:
            data = tomllib.load(palette_file)
        colors = data["colors"]
        if not isinstance(colors, dict):
            raise ValueError("colors must be a TOML table")
        return Palette(**{name: load_color(name, colors[name]) for name in ("off", "primary", "secondary", "warning", "fault", "controller_failure")})
    except (OSError, KeyError, tomllib.TOMLDecodeError, ValueError) as error:
        raise SystemExit(f"invalid RGB palette at {path}: {error}") from error


OFF = load_palette().off

def active_mode_name(device) -> str:
    return device.modes[device.active_mode].name


def ensure_mode(device, mode: str) -> None:
    if active_mode_name(device).lower() != mode.lower():
        device.set_mode(mode)


def set_rog_eye(board, color: RGBColor) -> None:
    ensure_mode(board, "Direct")
    zone = board.zones[ROG_EYE_ZONE]
    if len(zone.leds) < ROG_EYE_LED_COUNT:
        raise RuntimeError(f"expected at least {ROG_EYE_LED_COUNT} ROG-eye LEDs, found {len(zone.leds)}")
    zone.set_colors([color] * ROG_EYE_LED_COUNT + [OFF] * (len(zone.leds) - ROG_EYE_LED_COUNT), fast=True)


def set_ram_color(client, color: RGBColor) -> None:
    for device_index in PHYSICAL_RAM_DEVICE_ORDER:
        device = client.devices[device_index]
        ensure_mode(device, "Direct")
        device.set_colors([color] * len(device.leds), fast=True)


def level_bar(level: int, primary: RGBColor, secondary: RGBColor, led_count: int = RAM_LEDS_PER_MODULE) -> list[RGBColor]:
    return [primary] * (led_count - level) + [secondary] * level


def render_idle(client, palette: Palette) -> None:
    set_ram_color(client, palette.primary)
    set_rog_eye(client.devices[BOARD_DEVICE], palette.primary)


def render_working(client, palette: Palette, cpu: int, gpu: int, memory: int, task: int) -> None:
    for device_index, level in zip(PHYSICAL_RAM_DEVICE_ORDER, (cpu, gpu, memory, task), strict=True):
        device = client.devices[device_index]
        ensure_mode(device, "Direct")
        device.set_colors(level_bar(level, palette.primary, palette.secondary, len(device.leds)), fast=True)
    set_rog_eye(client.devices[BOARD_DEVICE], palette.secondary)


def render_working_progress(client, palette: Palette, completed: int) -> None:
    for module_position, device_index in enumerate(PHYSICAL_RAM_DEVICE_ORDER):
        module_completed = max(0, min(RAM_LEDS_PER_MODULE, completed - module_position * RAM_LEDS_PER_MODULE))
        device = client.devices[device_index]
        if len(device.leds) != RAM_LEDS_PER_MODULE:
            raise RuntimeError(f"expected {RAM_LEDS_PER_MODULE} LEDs on RAM device {device_index}, found {len(device.leds)}")
        ensure_mode(device, "Direct")
        device.set_colors(level_bar(module_completed, palette.primary, palette.secondary), fast=True)
    set_rog_eye(client.devices[BOARD_DEVICE], palette.secondary)


def overlay_warning(client, palette: Palette) -> None:
    set_rog_eye(client.devices[BOARD_DEVICE], palette.warning)


def render_fault_phase(client, palette: Palette, phase: int) -> None:
    for position, device_index in enumerate(PHYSICAL_RAM_DEVICE_ORDER):
        color = palette.fault if (position + phase) % 2 == 0 else palette.primary
        device = client.devices[device_index]
        ensure_mode(device, "Direct")
        device.set_colors([color] * len(device.leds), fast=True)
    set_rog_eye(client.devices[BOARD_DEVICE], palette.primary if phase % 2 == 0 else palette.fault)


def render_off(client) -> None:
    for device in client.devices:
        ensure_mode(device, "Off")


def manual_client() -> OpenRGBClient:
    client = OpenRGBClient("127.0.0.1", 6742, "Monolith RGB manual renderer")
    if len(client.devices) != 5:
        raise RuntimeError(f"expected 5 mapped controllers, found {len(client.devices)}")
    return client


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--apply", action="store_true", help="apply the requested render")
    parser.add_argument("--scene", choices=("idle", "working", "working-progress", "warning", "fault-phase", "off"), default="working")
    parser.add_argument("--palette", type=Path, default=DEFAULT_PALETTE_PATH)
    parser.add_argument("--phase", type=int, default=0, choices=(0, 1))
    for name in ("cpu", "gpu", "memory", "task"):
        parser.add_argument(f"--{name}", type=int, default=0, choices=range(9))
    parser.add_argument("--task-progress", type=int, default=0, choices=range(33))
    args = parser.parse_args()
    if not args.apply:
        parser.error("refusing to change LEDs without --apply")

    client = manual_client()
    palette = load_palette(args.palette)
    if args.scene == "idle":
        render_idle(client, palette)
    elif args.scene == "working":
        render_working(client, palette, args.cpu, args.gpu, args.memory, args.task)
    elif args.scene == "working-progress":
        render_working_progress(client, palette, args.task_progress)
    elif args.scene == "warning":
        overlay_warning(client, palette)
    elif args.scene == "fault-phase":
        render_fault_phase(client, palette, args.phase)
    else:
        render_off(client)
    print(f"{args.scene} render applied")


if __name__ == "__main__":
    main()
