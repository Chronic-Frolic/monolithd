#!/usr/bin/env python3
"""Local OpenRGB SDK-6 bridge for Monolith Events.

This bridge owns OpenRGB protocol negotiation and controller discovery. It is
intentionally separate from event semantics and has no network listener.
"""

from __future__ import annotations

import argparse
import asyncio
import json
import os
from pathlib import Path
import socket
import struct
from typing import Any

RUNTIME = Path(os.environ.get("XDG_RUNTIME_DIR", f"/run/user/{os.getuid()}")) / "monolith-events"
SOCKET_PATH = RUNTIME / "openrgb-sdk-bridge.sock"
OPENRGB_HOST = "127.0.0.1"
OPENRGB_PORT = 6742
PROTOCOL_VERSION = 6

PACKET_REQUEST_CONTROLLER_COUNT = 0
PACKET_REQUEST_PROTOCOL_VERSION = 40
PACKET_SET_CLIENT_NAME = 50
MAGIC = b"ORGB"
HEADER = struct.Struct("<4sIII")


def receive_exact(connection: socket.socket, size: int) -> bytes:
    data = b""
    while len(data) < size:
        chunk = connection.recv(size - len(data))
        if not chunk:
            raise ConnectionError("OpenRGB SDK closed the connection")
        data += chunk
    return data


def read_string(payload: bytes, offset: int) -> tuple[str, int]:
    size = struct.unpack_from("<H", payload, offset)[0]
    offset += 2
    return payload[offset:offset + size - 1].decode(errors="replace"), offset + size

def describe_controller(transport, controller_id: int) -> dict[str, Any]:
    transport.send(controller_id, 1, struct.pack("<I", PROTOCOL_VERSION))
    _, payload = transport.receive(1)
    offset = 4
    controller_type = struct.unpack_from("<i", payload, offset)[0]
    offset += 4
    name, offset = read_string(payload, offset)
    vendor, offset = read_string(payload, offset)
    description, offset = read_string(payload, offset)
    version, offset = read_string(payload, offset)
    serial, offset = read_string(payload, offset)
    location, offset = read_string(payload, offset)
    return {"id": controller_id, "type": controller_type, "name": name, "vendor": vendor, "description": description, "version": version, "serial": serial, "location": location}


class SDK6Transport:
    def __init__(self) -> None:
        self.connection = socket.create_connection((OPENRGB_HOST, OPENRGB_PORT), timeout=5)
        self.connection.settimeout(5)
        self.server_protocol = self.negotiate()
        if self.server_protocol < PROTOCOL_VERSION:
            raise RuntimeError(f"OpenRGB server negotiated protocol {self.server_protocol}, need SDK 6")
        self.send(0, PACKET_SET_CLIENT_NAME, b"Monolith OpenRGB SDK Bridge\0")

    def close(self) -> None:
        self.connection.close()

    def send(self, device_id: int, packet_id: int, payload: bytes = b"") -> None:
        self.connection.sendall(HEADER.pack(MAGIC, device_id, packet_id, len(payload)) + payload)

    def receive(self, expected_packet: int) -> tuple[int, bytes]:
        while True:
            magic, device_id, packet_id, packet_size = HEADER.unpack(receive_exact(self.connection, HEADER.size))
            if magic != MAGIC:
                raise RuntimeError("invalid OpenRGB SDK packet magic")
            payload = receive_exact(self.connection, packet_size)
            if packet_id == expected_packet:
                return device_id, payload

    def negotiate(self) -> int:
        self.send(0, PACKET_REQUEST_PROTOCOL_VERSION, struct.pack("<I", PROTOCOL_VERSION))
        _, payload = self.receive(PACKET_REQUEST_PROTOCOL_VERSION)
        if len(payload) != 4:
            raise RuntimeError(f"invalid protocol response size {len(payload)}")
        return struct.unpack("<I", payload)[0]

    def discover(self) -> dict[str, Any]:
        self.send(0, PACKET_REQUEST_CONTROLLER_COUNT)
        _, payload = self.receive(PACKET_REQUEST_CONTROLLER_COUNT)
        if len(payload) < 4:
            raise RuntimeError("invalid controller-count response")
        count = struct.unpack_from("<I", payload)[0]
        expected = 4 + count * 4
        if len(payload) != expected:
            raise RuntimeError(f"SDK-6 controller ID response size {len(payload)} does not match {count} controllers")
        ids = list(struct.unpack_from(f"<{count}I", payload, 4))
        return {"protocol": self.server_protocol, "controller_count": count, "controller_ids": ids, "controllers": [describe_controller(self, controller_id) for controller_id in ids]}


def request(payload: dict[str, Any]) -> dict[str, Any]:
    action = payload.get("action", "discover")
    if action not in ("health", "discover"):
        raise ValueError(f"unsupported bridge action: {action}")
    transport = SDK6Transport()
    try:
        return {"ok": True, **transport.discover()}
    finally:
        transport.close()


async def serve() -> None:
    RUNTIME.mkdir(mode=0o700, parents=True, exist_ok=True)
    if SOCKET_PATH.exists():
        SOCKET_PATH.unlink()

    async def client(reader: asyncio.StreamReader, writer: asyncio.StreamWriter) -> None:
        try:
            message = json.loads((await reader.readline()).decode())
            response = request(message)
        except Exception as error:
            response = {"ok": False, "error": str(error)}
        writer.write((json.dumps(response, sort_keys=True) + "\n").encode())
        await writer.drain()
        writer.close()
        await writer.wait_closed()

    server = await asyncio.start_unix_server(client, path=str(SOCKET_PATH))
    SOCKET_PATH.chmod(0o600)
    async with server:
        await server.serve_forever()


def call_bridge(action: str) -> dict[str, Any]:
    with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as client:
        client.settimeout(10)
        client.connect(str(SOCKET_PATH))
        client.sendall((json.dumps({"action": action}) + "\n").encode())
        response = b""
        while not response.endswith(b"\n"):
            response += client.recv(65536)
    return json.loads(response)


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("command", choices=("serve", "health", "discover"))
    args = parser.parse_args()
    if args.command == "serve":
        asyncio.run(serve())
    else:
        print(json.dumps(call_bridge(args.command), indent=2, sort_keys=True))


if __name__ == "__main__":
    main()
