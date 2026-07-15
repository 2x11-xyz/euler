"""A dependency-free JSON-RPC stdio client for Euler process extensions.

The protocol is deliberately small and language-neutral. This module is an
ergonomic Python client, not a Python-specific runtime mode.
"""

from __future__ import annotations

import base64
import json
import math
import sys
from collections.abc import Callable, Mapping
from dataclasses import dataclass
from typing import Any, Optional


PROTOCOL_VERSION = "euler-managed-process/1"
_DEFAULT_MAX_MESSAGE_BYTES = 1024 * 1024


class ProtocolError(RuntimeError):
    """The host or peer sent a message outside the managed-process contract."""


class HostError(RuntimeError):
    """A capability-gated host operation did not succeed."""


class Cancelled(RuntimeError):
    """Euler cancelled the in-flight command before it completed."""


@dataclass(frozen=True)
class CommandContext:
    """The JSON input supplied to one declared extension command."""

    command: str
    input: Any
    host: "Host"


CommandHandler = Callable[[CommandContext], Mapping[str, Any]]


class _Wire:
    def __init__(self) -> None:
        self._max_message_bytes = _DEFAULT_MAX_MESSAGE_BYTES

    def read(self) -> dict[str, Any]:
        # The Rust host limits the JSON payload, not its terminating newline.
        # Ask for two bytes beyond the payload cap so both a legal boundary
        # frame and an overlong frame are unambiguous.
        line = sys.stdin.buffer.readline(self._max_message_bytes + 2)
        if (
            not line
            or len(line) > self._max_message_bytes + 1
            or not line.endswith(b"\n")
        ):
            raise ProtocolError("invalid protocol framing")
        try:
            message = json.loads(line)
        except (TypeError, ValueError) as error:
            raise ProtocolError("invalid protocol message") from error
        if not isinstance(message, dict) or message.get("jsonrpc") != "2.0":
            raise ProtocolError("invalid protocol message")
        return message

    def write(self, message: Mapping[str, Any]) -> None:
        encoded = json.dumps(message, separators=(",", ":"), ensure_ascii=False).encode("utf-8")
        if len(encoded) > self._max_message_bytes:
            raise ProtocolError("protocol message exceeds host limit")
        sys.stdout.buffer.write(encoded + b"\n")
        sys.stdout.buffer.flush()

    def set_max_message_bytes(self, maximum: Any) -> None:
        if isinstance(maximum, int) and 0 < maximum <= _DEFAULT_MAX_MESSAGE_BYTES:
            self._max_message_bytes = maximum


class Host:
    """Capability-gated host APIs available during a command invocation."""

    def __init__(self, wire: _Wire) -> None:
        self._wire = wire
        self._next_request_id = 1

    def progress(self, message: str, fraction: Optional[float] = None) -> None:
        if not isinstance(message, str) or not message or len(message.encode("utf-8")) > 4096:
            raise ValueError("progress message must be 1..4096 UTF-8 bytes")
        params: dict[str, Any] = {"message": message}
        if fraction is not None:
            if (
                isinstance(fraction, bool)
                or not isinstance(fraction, (int, float))
                or not math.isfinite(fraction)
                or not 0 <= fraction <= 1
            ):
                raise ValueError("progress fraction must be between 0 and 1")
            params["fraction"] = fraction
        self._wire.write({"jsonrpc": "2.0", "method": "euler/progress", "params": params})

    def query_provenance(
        self,
        *,
        after_event_id: Optional[str] = None,
        kinds: Optional[list[str]] = None,
        limit: int = 128,
        scan_limit: int = 1024,
        include_blob_fields: bool = False,
        blob_byte_limit: int = 1024 * 1024,
    ) -> dict[str, Any]:
        return self._request(
            "euler/host/query-provenance",
            {
                "after_event_id": after_event_id,
                "kinds": kinds or [],
                "limit": limit,
                "scan_limit": scan_limit,
                "include_blob_fields": include_blob_fields,
                "blob_byte_limit": blob_byte_limit,
            },
        )

    def read_diagnostics(self, *, tail_lines: int, max_bytes: int) -> dict[str, Any]:
        return self._request(
            "euler/host/read-diagnostics",
            {"tail_lines": tail_lines, "max_bytes": max_bytes},
        )

    def state_dir(self) -> str:
        result = self._request("euler/host/state-dir", {})
        path = result.get("path") if isinstance(result, dict) else None
        if not isinstance(path, str):
            raise ProtocolError("host returned an invalid state directory")
        return path

    def write_artifact(
        self,
        *,
        display_name: str,
        media_type: str,
        data: bytes,
        source_event_ids: Optional[list[str]] = None,
        metadata: Optional[Mapping[str, Any]] = None,
    ) -> dict[str, Any]:
        if not isinstance(data, bytes):
            raise TypeError("artifact data must be bytes")
        return self._request(
            "euler/host/write-artifact",
            {
                "display_name": display_name,
                "media_type": media_type,
                "bytes_base64": base64.b64encode(data).decode("ascii"),
                "source_event_ids": source_event_ids or [],
                "metadata": dict(metadata or {}),
            },
        )

    def load_checkpoint(self, name: str) -> Optional[dict[str, Any]]:
        return self._request("euler/host/load-checkpoint", {"name": name})

    def store_checkpoint(self, name: str, checkpoint: Mapping[str, Any]) -> None:
        self._request(
            "euler/host/store-checkpoint",
            {"name": name, "checkpoint": dict(checkpoint)},
        )

    def record_agent_task_result(
        self, task: Mapping[str, Any], result: Mapping[str, Any]
    ) -> dict[str, Any]:
        return self._request(
            "euler/host/record-agent-task-result",
            {"task": dict(task), "result": dict(result)},
        )

    def update_context_slot(self, slot: str, content: str) -> None:
        self._request(
            "euler/host/update-context-slot",
            {"slot": slot, "content": content},
        )

    def spawn_agent(self, task: Mapping[str, Any]) -> dict[str, Any]:
        return self._request("euler/host/spawn-agent", dict(task))

    def spawn_agents(self, tasks: list[Mapping[str, Any]]) -> list[dict[str, Any]]:
        result = self._request("euler/host/spawn-agents", {"tasks": [dict(task) for task in tasks]})
        if not isinstance(result, list):
            raise ProtocolError("host returned invalid agent outcomes")
        return result

    def _request(self, method: str, params: Mapping[str, Any]) -> Any:
        request_id = f"client-{self._next_request_id}"
        self._next_request_id += 1
        self._wire.write(
            {"jsonrpc": "2.0", "id": request_id, "method": method, "params": dict(params)}
        )
        while True:
            message = self._wire.read()
            if message.get("id") == request_id:
                if "result" in message and "error" not in message:
                    return message["result"]
                error = message.get("error")
                if isinstance(error, dict) and isinstance(error.get("message"), str):
                    raise HostError(error["message"])
                raise HostError("host operation failed")
            if message.get("method") == "$/cancelRequest":
                raise Cancelled("Euler cancelled the command")
            raise ProtocolError("unexpected message while waiting for host response")


def serve(handlers: Mapping[str, CommandHandler]) -> None:
    """Run declared command handlers until Euler sends the clean exit signal.

    Each handler must return a JSON object. Exceptions are intentionally not
    serialized to Euler: process stderr and implementation details never enter
    the model canvas or provenance as extension output.
    """

    wire = _Wire()
    initialize = wire.read()
    _require_request(initialize, "initialize")
    params = initialize.get("params")
    if not isinstance(params, dict) or PROTOCOL_VERSION not in params.get("protocol_versions", []):
        _error(wire, initialize["id"], -32602, "no compatible protocol version")
        return
    limits = params.get("limits")
    if isinstance(limits, dict):
        wire.set_max_message_bytes(limits.get("max_message_bytes"))
    _result(wire, initialize["id"], {"protocol_version": PROTOCOL_VERSION})

    initialized = wire.read()
    if initialized.get("method") != "initialized" or "id" in initialized:
        raise ProtocolError("expected initialized notification")

    command = wire.read()
    _require_request(command, "euler/command")
    command_params = command.get("params")
    if not isinstance(command_params, dict) or not isinstance(command_params.get("command"), str):
        _error(wire, command["id"], -32602, "invalid command request")
    else:
        name = command_params["command"]
        handler = handlers.get(name)
        if handler is None:
            _error(wire, command["id"], -32601, "unknown extension command")
        else:
            try:
                result = handler(CommandContext(name, command_params.get("input"), Host(wire)))
                if not isinstance(result, Mapping):
                    raise TypeError("command result must be an object")
                _result(wire, command["id"], dict(result))
            except Cancelled:
                _error(wire, command["id"], -32800, "extension command cancelled")
            except Exception:
                _error(wire, command["id"], -32000, "extension command failed")

    shutdown = wire.read()
    _require_request(shutdown, "shutdown")
    _result(wire, shutdown["id"], {})
    exit_message = wire.read()
    if exit_message.get("method") != "exit" or "id" in exit_message:
        raise ProtocolError("expected exit notification")


def _require_request(message: Mapping[str, Any], method: str) -> None:
    if message.get("method") != method or not isinstance(message.get("id"), (str, int)):
        raise ProtocolError(f"expected {method} request")


def _result(wire: _Wire, request_id: Any, result: Any) -> None:
    wire.write({"jsonrpc": "2.0", "id": request_id, "result": result})


def _error(wire: _Wire, request_id: Any, code: int, message: str) -> None:
    wire.write({"jsonrpc": "2.0", "id": request_id, "error": {"code": code, "message": message}})
