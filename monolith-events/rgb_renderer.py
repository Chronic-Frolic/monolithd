#!/usr/bin/env python3
"""Render explicit Monolith Event Controller RGB state through OpenRGB SDK.

This first renderer deliberately accepts manually supplied levels. It has no
metrics collection, task inference, scheduler, or power-policy authority.
"""

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


def level_bar(level: int, color: RGBColor, led_count: int = 8) -> list[RGBColor]:
    """Return a bottom-to-top full-brightness bar with level illuminated LEDs."""
    return [OFF] * (led_count - level) + [color] * level


def set_rog_eye_normal(board) -> None:
    """Set the ROG eye white while retaining unmapped header LEDs off."""
    board.set_mode("Direct")
    zone = board.zones[ROG_EYE_ZONE]
    if len(zone.leds) < ROG_EYE_LED_COUNT:
        raise RuntimeError(
            f"expected at least {ROG_EYE_LED_COUNT} ROG-eye LEDs, found {len(zone.leds)}"
        )
    zone.set_colors(
        [WHITE] * ROG_EYE_LED_COUNT + [OFF] * (len(zone.leds) - ROG_EYE_LED_COUNT)
    )


def render_working(client, cpu: int, gpu: int, memory: int, task: int) -> None:
    """Render the approved full-brightness working-state language.

    Physical RAM left-to-right: CPU white, GPU white, memory white, task green.
    The ROG eye is the general normal-status indicator and is white.
    """
    levels = (cpu, gpu, memory, task)
    colors = (WHITE, WHITE, WHITE, GREEN)
    for device_index, level, color in zip(
        PHYSICAL_RAM_DEVICE_ORDER, levels, colors, strict=True
    ):
        device = client.devices[device_index]
        device.set_mode("Direct")
        device.set_colors(level_bar(level, color, len(device.leds)))
    set_rog_eye_normal(client.devices[BOARD_DEVICE])


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--apply", action="store_true", help="apply the requested render")
    parser.add_argument("--scene", choices=("working",), default="working")
    for name, description in (
        ("cpu", "CPU utilization bar"),
        ("gpu", "GPU utilization bar"),
        ("memory", "memory utilization bar"),
        ("task", "tracked-task progress/state bar"),
    ):
        parser.add_argument(
            f"--{name}",
            type=int,
            default=0,
            choices=range(9),
            help=f"0-8 LEDs: {description}",
        )
    args = parser.parse_args()
    if not args.apply:
        parser.error("refusing to change LEDs without --apply")

    client = OpenRGBClient("127.0.0.1", 6742, "Monolith Event Controller")
    if len(client.devices) != 5:
        raise SystemExit(f"expected 5 mapped controllers, found {len(client.devices)}")
    render_working(client, args.cpu, args.gpu, args.memory, args.task)
    print(
        "working render applied: "
        f"cpu={args.cpu}/8 gpu={args.gpu}/8 memory={args.memory}/8 task={args.task}/8"
    )


if __name__ == "__main__":
    main()
