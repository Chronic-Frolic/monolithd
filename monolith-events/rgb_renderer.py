#!/usr/bin/env python3
"""Render explicit Monolith Event Controller RGB state through OpenRGB SDK."""

from __future__ import annotations

import argparse

from openrgb import OpenRGBClient
from openrgb.utils import RGBColor

OFF = RGBColor(0, 0, 0)
WHITE = RGBColor(255, 255, 255)
GREEN = RGBColor(0, 255, 0)

# Physical RAM left-to-right, as verified 2026-09-19.
PHYSICAL_RAM_DEVICE_ORDER = (1, 3, 0, 2)
BOARD_DEVICE = 4
ROG_EYE_ZONE = 0
ROG_EYE_LED_COUNT = 3
RAM_LEDS_PER_MODULE = 8


def level_bar(level: int, color: RGBColor, led_count: int = RAM_LEDS_PER_MODULE) -> list[RGBColor]:
    """Return a bottom-to-top full-brightness bar with level illuminated LEDs."""
    return [OFF] * (led_count - level) + [color] * level


def task_progress_segment(completed: int) -> list[RGBColor]:
    """Return one RAM module's white-to-green bottom-to-top progress segment."""
    return [WHITE] * (RAM_LEDS_PER_MODULE - completed) + [GREEN] * completed


def set_rog_eye_normal(board) -> None:
    """Set the ROG eye white while retaining unmapped header LEDs off."""
    board.set_mode("Direct")
    zone = board.zones[ROG_EYE_ZONE]
    if len(zone.leds) < ROG_EYE_LED_COUNT:
        raise RuntimeError(
            f"expected at least {ROG_EYE_LED_COUNT} ROG-eye LEDs, found {len(zone.leds)}"
        )
    zone.set_colors([WHITE] * ROG_EYE_LED_COUNT + [OFF] * (len(zone.leds) - ROG_EYE_LED_COUNT))


def render_working(client, cpu: int, gpu: int, memory: int, task: int) -> None:
    """Render the approved full-brightness per-resource working-state language."""
    levels = (cpu, gpu, memory, task)
    colors = (WHITE, WHITE, WHITE, GREEN)
    for device_index, level, color in zip(PHYSICAL_RAM_DEVICE_ORDER, levels, colors, strict=True):
        device = client.devices[device_index]
        device.set_mode("Direct")
        device.set_colors(level_bar(level, color, len(device.leds)))
    set_rog_eye_normal(client.devices[BOARD_DEVICE])


def render_task_progress(client, completed: int) -> None:
    """Render one 32-segment task-progress bar across physical RAM left-to-right."""
    for module_position, device_index in enumerate(PHYSICAL_RAM_DEVICE_ORDER):
        module_completed = max(0, min(RAM_LEDS_PER_MODULE, completed - module_position * RAM_LEDS_PER_MODULE))
        device = client.devices[device_index]
        if len(device.leds) != RAM_LEDS_PER_MODULE:
            raise RuntimeError(
                f"expected {RAM_LEDS_PER_MODULE} LEDs on RAM device {device_index}, found {len(device.leds)}"
            )
        device.set_mode("Direct")
        device.set_colors(task_progress_segment(module_completed))
    set_rog_eye_normal(client.devices[BOARD_DEVICE])


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--apply", action="store_true", help="apply the requested render")
    parser.add_argument("--scene", choices=("working", "task-progress"), default="working")
    for name, description in (
        ("cpu", "CPU utilization bar"),
        ("gpu", "GPU utilization bar"),
        ("memory", "memory utilization bar"),
        ("task", "tracked-task progress/state bar"),
    ):
        parser.add_argument(f"--{name}", type=int, default=0, choices=range(9), help=f"0-8 LEDs: {description}")
    parser.add_argument(
        "--task-progress",
        type=int,
        default=0,
        choices=range(33),
        help="0-32 completed segments for the all-RAM task-progress scene",
    )
    args = parser.parse_args()
    if not args.apply:
        parser.error("refusing to change LEDs without --apply")

    client = OpenRGBClient("127.0.0.1", 6742, "Monolith Event Controller")
    if len(client.devices) != 5:
        raise SystemExit(f"expected 5 mapped controllers, found {len(client.devices)}")
    if args.scene == "working":
        render_working(client, args.cpu, args.gpu, args.memory, args.task)
        print(f"working render applied: cpu={args.cpu}/8 gpu={args.gpu}/8 memory={args.memory}/8 task={args.task}/8")
    else:
        render_task_progress(client, args.task_progress)
        print(f"task-progress render applied: {args.task_progress}/32 complete")


if __name__ == "__main__":
    main()
