"""Tiny dependency-free peer for exercising Euler's JSON-RPC wire.

This is deliberately a protocol fixture, not a language SDK. Production
Python bindings live in the euler-extensions repository.
"""

import json
import sys


PROTOCOL_VERSION = "euler-managed-process/1"
DEFAULT_MAX_MESSAGE_BYTES = 1024 * 1024


class Cancelled(Exception):
    pass


class Peer:
    def __init__(self):
        self.max_message_bytes = DEFAULT_MAX_MESSAGE_BYTES
        self.next_request_id = 1

    def read(self):
        line = sys.stdin.buffer.readline(self.max_message_bytes + 2)
        if (
            not line
            or len(line) > self.max_message_bytes + 1
            or not line.endswith(b"\n")
        ):
            raise RuntimeError("invalid protocol framing")
        message = json.loads(line)
        if not isinstance(message, dict) or message.get("jsonrpc") != "2.0":
            raise RuntimeError("invalid protocol message")
        return message

    def write(self, message):
        encoded = json.dumps(
            message, separators=(",", ":"), ensure_ascii=False
        ).encode("utf-8")
        if len(encoded) > self.max_message_bytes:
            raise RuntimeError("protocol message exceeds host limit")
        sys.stdout.buffer.write(encoded + b"\n")
        sys.stdout.buffer.flush()

    def request(self, method, params):
        request_id = f"peer-{self.next_request_id}"
        self.next_request_id += 1
        self.write(
            {
                "jsonrpc": "2.0",
                "id": request_id,
                "method": method,
                "params": params,
            }
        )
        while True:
            message = self.read()
            if message.get("id") == request_id:
                if "result" in message and "error" not in message:
                    return message["result"]
                raise RuntimeError("host operation failed")
            if message.get("method") == "$/cancelRequest":
                raise Cancelled()
            raise RuntimeError("unexpected message")

    def progress(self, message, fraction=None):
        params = {"message": message}
        if fraction is not None:
            params["fraction"] = fraction
        self.write(
            {"jsonrpc": "2.0", "method": "euler/progress", "params": params}
        )


def serve(handlers):
    peer = Peer()
    initialize = peer.read()
    params = initialize["params"]
    if PROTOCOL_VERSION not in params["protocol_versions"]:
        raise RuntimeError("no compatible protocol version")
    limit = params.get("limits", {}).get("max_message_bytes")
    if isinstance(limit, int) and 0 < limit <= DEFAULT_MAX_MESSAGE_BYTES:
        peer.max_message_bytes = limit
    peer.write(
        {
            "jsonrpc": "2.0",
            "id": initialize["id"],
            "result": {"protocol_version": PROTOCOL_VERSION},
        }
    )
    assert peer.read()["method"] == "initialized"
    command = peer.read()
    command_params = command["params"]
    try:
        result = handlers[command_params["command"]](
            peer, command_params.get("input")
        )
        if not isinstance(result, dict):
            raise RuntimeError("command result must be an object")
        response = {"jsonrpc": "2.0", "id": command["id"], "result": result}
    except Cancelled:
        response = {
            "jsonrpc": "2.0",
            "id": command["id"],
            "error": {"code": -32800, "message": "extension command cancelled"},
        }
    except Exception:
        response = {
            "jsonrpc": "2.0",
            "id": command["id"],
            "error": {"code": -32000, "message": "extension command failed"},
        }
    peer.write(response)
    shutdown = peer.read()
    assert shutdown["method"] == "shutdown"
    peer.write({"jsonrpc": "2.0", "id": shutdown["id"], "result": {}})
    assert peer.read()["method"] == "exit"
