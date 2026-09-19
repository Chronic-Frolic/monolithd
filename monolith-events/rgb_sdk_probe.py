#!/usr/bin/env python3
"""Read-only inventory of controllers exposed by White Monolith's OpenRGB SDK."""

from __future__ import annotations

import json

from openrgb import OpenRGBClient


def main() -> None:
    client = OpenRGBClient("127.0.0.1", 6742, "Monolith Event Controller SDK probe")
    devices = []
    for index, device in enumerate(client.devices):
        devices.append(
            {
                "index": index,
                "name": device.name,
                "type": str(device.type),
                "zones": [zone.name for zone in device.zones],
                "led_count": len(device.leds),
            }
        )
    print(json.dumps({"device_count": len(devices), "devices": devices}, indent=2))


if __name__ == "__main__":
    main()
