#!/usr/bin/env python3
"""Manual, non-semantic OpenRGB SDK diagnostics for White Monolith."""

from __future__ import annotations

import argparse

from openrgb import OpenRGBClient
from openrgb.utils import RGBColor

OFF = RGBColor(0, 0, 0)
WHITE = RGBColor(255, 255, 255)
RAM_PATTERN_COLORS = {
    0: RGBColor(0, 255, 255),    # SDK device 0, physical RAM third from left
    1: RGBColor(0, 255, 128),    # SDK device 1, physical RAM first from left
    2: RGBColor(128, 0, 255),    # SDK device 2, physical RAM fourth from left
    3: RGBColor(255, 176, 0),    # SDK device 3, physical RAM second from left
}
RAM_ORDER_COLORS = {
    0: RGBColor(255, 0, 0),
    1: RGBColor(0, 255, 0),
    2: RGBColor(0, 96, 255),
    3: RGBColor(255, 210, 0),
}


def alternating(color: RGBColor) -> list[RGBColor]:
    return [color if index % 2 == 0 else OFF for index in range(8)]


def turn_board_off(board) -> None:
    board.set_mode("Direct")
    board.zones[0].set_colors([OFF] * len(board.zones[0].leds))


def apply_pattern(client) -> None:
    for index, color in RAM_PATTERN_COLORS.items():
        client.devices[index].set_mode("Direct")
        client.devices[index].set_colors(alternating(color))
    client.devices[4].set_mode("Direct")
    client.devices[4].zones[0].set_colors([WHITE] * len(client.devices[4].zones[0].leds))


def apply_ram_order(client) -> None:
    for index, color in RAM_ORDER_COLORS.items():
        client.devices[index].set_mode("Direct")
        client.devices[index].set_color(color)
    turn_board_off(client.devices[4])


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--apply", action="store_true", help="apply the requested diagnostic")
    parser.add_argument("--scene", choices=("pattern", "ram-order"), default="pattern")
    args = parser.parse_args()
    if not args.apply:
        parser.error("refusing to change LEDs without --apply")

    client = OpenRGBClient("127.0.0.1", 6742, "Monolith SDK diagnostic")
    if len(client.devices) != 5:
        raise SystemExit(f"expected 5 mapped controllers, found {len(client.devices)}")
    if args.scene == "pattern":
        apply_pattern(client)
    else:
        apply_ram_order(client)
    print(f"SDK diagnostic scene applied: {args.scene}")


if __name__ == "__main__":
    main()
