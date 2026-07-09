#!/usr/bin/env python3
# Archived prototype (2026-07-09): the pre-Rust Code Swarm this repo's
# euler-extension-code-swarm should be measured against. Provided by Eli from
# the euler-old tree; kept verbatim below the original docstring. See
# docs/reviews/extension-ux-code-swarm-2026-07-09.md ("What it should do")
# for the gap analysis. Not built, not shipped, review-only by design.
"""Repo-local Code Swarm prototype for Codex-driven review.

This is intentionally review-only: it reads local/PR context, calls OpenRouter
models in parallel, and prints feedback. It never edits source files or posts
comments back to GitHub.
"""

from __future__ import annotations

import argparse
import concurrent.futures
import dataclasses
import datetime as dt
import json
import os
import pathlib
import selectors
import socket
import subprocess
import sys
import tempfile
import time
import urllib.error
import urllib.request
import uuid
from typing import Any


DEFAULT_MODELS = [
    "anthropic/claude-fable-5",
    "z-ai/glm-5.2",
    "openai/gpt-5.4",
]

OPENROUTER_URL = "https://openrouter.ai/api/v1/chat/completions"
REFERER = "https://github.com/2x11-xyz/euler"
TITLE = "Euler Codex Swarm"
CONTEXT_OVERHEAD_BYTES = 8_000
HOST_ABI_VERSION = 1
HELPER_EXTENSION_ID = "code_swarm"
HELPER_EXTENSION_NAME = "Code Swarm"
HELPER_EXTENSION_VERSION = "0.1.0"
HELPER_MAX_ARTIFACT_BYTES = 1_000_000
HELPER_SELF_TEST_TIMEOUT_SECONDS = 10
HELPER_SELF_TEST_MAX_OUTPUT_BYTES = 2_048
HELPER_SELF_TEST_MAX_PROTOCOL_LINE_BYTES = 1_200_000


@dataclasses.dataclass
class SwarmConfig:
    models: list[str]
    temperature: float
    max_tokens: int
    timeout_seconds: int
    max_file_bytes: int
    max_total_bytes: int


@dataclasses.dataclass
class ReviewContext:
    mode: str
    prompt: str
    body: str
    skipped: list[str]


class MissingKeyError(RuntimeError):
    pass


def repo_root() -> pathlib.Path:
    try:
        out = subprocess.check_output(
            ["git", "rev-parse", "--show-toplevel"],
            text=True,
            stderr=subprocess.DEVNULL,
        ).strip()
        return pathlib.Path(out)
    except (subprocess.CalledProcessError, FileNotFoundError):
        return pathlib.Path.cwd()


def load_config(path: str | None) -> SwarmConfig:
    data: dict[str, Any] = {}
    candidates: list[pathlib.Path] = []
    if path:
        candidates.append(pathlib.Path(path))
    else:
        root = repo_root()
        candidates.extend(
            [
                root / ".codex-swarm.json",
                pathlib.Path.home() / ".codex-swarm.json",
            ]
        )

    for candidate in candidates:
        if candidate.exists():
            try:
                with candidate.open("r", encoding="utf-8") as f:
                    data = json.load(f)
            except OSError as exc:
                raise SystemExit(f"Could not read config {candidate}: {exc}") from exc
            except json.JSONDecodeError as exc:
                raise SystemExit(f"Invalid JSON in config {candidate}: {exc}") from exc
            break

    try:
        config = SwarmConfig(
            models=list(data.get("models", DEFAULT_MODELS)),
            temperature=float(data.get("temperature", 0.2)),
            max_tokens=int(data.get("max_tokens", 4096)),
            timeout_seconds=int(data.get("timeout_seconds", 60)),
            max_file_bytes=int(data.get("max_file_bytes", 100_000)),
            max_total_bytes=int(data.get("max_total_bytes", 500_000)),
        )
    except (TypeError, ValueError) as exc:
        raise SystemExit(f"Invalid config value: {exc}") from exc
    validate_config(config)
    return config


def validate_config(config: SwarmConfig) -> None:
    if not config.models:
        raise SystemExit("At least one model is required.")
    if len(config.models) > 5:
        raise SystemExit("Refusing to run more than 5 models in one swarm.")
    if config.max_tokens <= 0:
        raise SystemExit("max_tokens must be positive.")
    if config.timeout_seconds <= 0:
        raise SystemExit("timeout_seconds must be positive.")
    if config.max_file_bytes <= 0:
        raise SystemExit("max_file_bytes must be positive.")
    if config.max_total_bytes <= CONTEXT_OVERHEAD_BYTES:
        raise SystemExit(f"max_total_bytes must be greater than {CONTEXT_OVERHEAD_BYTES}.")


def load_openrouter_key() -> str:
    key = os.environ.get("OPENROUTER_API_KEY", "").strip()
    if key:
        return key

    secrets_path = pathlib.Path.home() / ".euler" / "secrets.json"
    if secrets_path.exists():
        try:
            with secrets_path.open("r", encoding="utf-8") as f:
                data = json.load(f)
        except OSError as exc:
            raise SystemExit(f"Could not read {secrets_path}: {exc}") from exc
        except json.JSONDecodeError as exc:
            raise SystemExit(f"Invalid JSON in {secrets_path}: {exc}") from exc
        key = str(data.get("OPENROUTER_API_KEY", "")).strip()
        if key:
            return key

    raise SystemExit(
        "OPENROUTER_API_KEY is not set. Set it in the environment or ~/.euler/secrets.json."
    )


def run_command(args: list[str]) -> str:
    try:
        return subprocess.check_output(args, text=True, stderr=subprocess.STDOUT)
    except FileNotFoundError as exc:
        raise SystemExit(f"Required command not found: {args[0]}") from exc
    except subprocess.CalledProcessError as exc:
        output = exc.output.strip()
        raise SystemExit(f"Command failed: {' '.join(args)}\n{output}") from exc


def utf8_len(text: str) -> int:
    return len(text.encode("utf-8"))


def truncate_text(label: str, text: str, limit: int, skipped: list[str]) -> str:
    if limit <= 0:
        skipped.append(f"{label} omitted because byte limit was {limit}")
        return ""
    if utf8_len(text) <= limit:
        return text
    encoded = text.encode("utf-8")
    skipped.append(f"{label} truncated from {len(encoded)} bytes to {limit} bytes")
    return encoded[:limit].decode("utf-8", errors="ignore")


def enforce_context_budget(context: ReviewContext, config: SwarmConfig) -> ReviewContext:
    body_limit = config.max_total_bytes - CONTEXT_OVERHEAD_BYTES
    before, marker, after = context.body.rpartition("Review prompt:")
    if marker and utf8_len(context.body) > body_limit:
        prompt_tail = marker + after
        prefix_limit = body_limit - utf8_len(prompt_tail) - 2
        if prefix_limit > 0:
            body = (
                truncate_text("assembled review context", before, prefix_limit, context.skipped)
                + "\n\n"
                + prompt_tail
            )
        else:
            body = truncate_text("assembled review context", context.body, body_limit, context.skipped)
    else:
        body = truncate_text("assembled review context", context.body, body_limit, context.skipped)
    return ReviewContext(
        mode=context.mode,
        prompt=context.prompt,
        body=body,
        skipped=context.skipped,
    )


def read_files(
    paths: list[str],
    config: SwarmConfig,
    *,
    repo_relative: bool = False,
) -> tuple[str, list[str]]:
    chunks: list[str] = []
    skipped: list[str] = []
    total = 0
    root = repo_root()

    for raw_path in paths:
        path = pathlib.Path(raw_path)
        if not path.is_absolute():
            root_path = root / path
            if repo_relative:
                path = root_path
            else:
                cwd_path = pathlib.Path.cwd() / path
                path = cwd_path if cwd_path.exists() else root_path
        if not path.exists():
            skipped.append(f"{raw_path} (missing)")
            continue
        if path.is_dir():
            skipped.append(f"{raw_path} (directory)")
            continue

        size = path.stat().st_size
        header = f"\n--- {raw_path} ---\n"
        overhead = utf8_len(header) + 1
        if size > config.max_file_bytes:
            skipped.append(f"{raw_path} ({size} bytes > {config.max_file_bytes} limit)")
            continue
        if total + size + overhead > config.max_total_bytes:
            skipped.append(f"{raw_path} (total size limit)")
            continue

        data = path.read_bytes()
        if b"\x00" in data[:512]:
            skipped.append(f"{raw_path} (binary)")
            continue
        content = data.decode("utf-8", errors="replace")
        chunks.append(f"{header}{content}\n")
        total += len(data) + overhead

    return "".join(chunks), skipped


def build_plan_context(prompt: str, config: SwarmConfig) -> ReviewContext:
    return enforce_context_budget(
        ReviewContext(mode="plan", prompt=prompt, body=prompt, skipped=[]),
        config,
    )


def build_code_context(args: argparse.Namespace, config: SwarmConfig) -> ReviewContext:
    content, skipped = read_files(args.files, config)
    body = f"Review the following local files:\n{content}\n\nReview prompt: {args.prompt}"
    return enforce_context_budget(
        ReviewContext(mode="review-code", prompt=args.prompt, body=body, skipped=skipped),
        config,
    )


def build_diff_context(args: argparse.Namespace, config: SwarmConfig) -> ReviewContext:
    if args.staged:
        diff_args = ["git", "diff", "--cached", "--patch"]
        label = "staged diff"
    elif args.base:
        diff_args = ["git", "diff", "--patch", f"{args.base}...HEAD"]
        label = f"diff against {args.base}"
    else:
        diff_args = ["git", "diff", "--patch"]
        label = "working tree diff"

    skipped: list[str] = []
    diff = truncate_text(label, run_command(diff_args), config.max_total_bytes, skipped)
    body = f"Review the following {label}:\n\n{diff}\n\nReview prompt: {args.prompt}"
    return enforce_context_budget(
        ReviewContext(mode="review-diff", prompt=args.prompt, body=body, skipped=skipped),
        config,
    )


def build_pr_context(args: argparse.Namespace, config: SwarmConfig) -> ReviewContext:
    pr = None if args.current else args.pr
    view_args = ["gh", "pr", "view"]
    if pr:
        view_args.append(pr)
    view_args.extend(
        [
            "--json",
            "number,title,body,author,baseRefName,headRefName,url,files,commits,reviews,comments",
        ]
    )

    skipped: list[str] = []
    try:
        metadata = json.loads(run_command(view_args))
    except json.JSONDecodeError as exc:
        raise SystemExit(f"gh pr view did not return valid JSON: {exc}") from exc

    diff_args = ["gh", "pr", "diff"]
    if pr:
        diff_args.append(pr)
    diff_args.append("--patch")
    diff_limit = min(args.max_diff_bytes, config.max_total_bytes - CONTEXT_OVERHEAD_BYTES)
    diff = truncate_text("PR diff", run_command(diff_args), diff_limit, skipped)

    file_context = ""
    if args.include_full_files:
        paths = [item["path"] for item in metadata.get("files", []) if item.get("path")]
        file_context, file_skipped = read_files(paths, config, repo_relative=True)
        skipped.extend(file_skipped)
        skipped.append(
            "full-file context was read from the local checkout; ensure it matches the PR head"
        )

    comments = ""
    if args.include_comments:
        comments = truncate_text(
            "PR comments/reviews",
            json.dumps(
                {
                    "reviews": metadata.get("reviews", []),
                    "comments": metadata.get("comments", []),
                },
                indent=2,
            ),
            max(1, config.max_total_bytes // 5),
            skipped,
        )

    summary = {
        "number": metadata.get("number"),
        "title": metadata.get("title"),
        "url": metadata.get("url"),
        "author": (metadata.get("author") or {}).get("login"),
        "base": metadata.get("baseRefName"),
        "head": metadata.get("headRefName"),
        "files": metadata.get("files", []),
        "commits": metadata.get("commits", []),
        "body": metadata.get("body"),
    }

    body = [
        "Review the following GitHub pull request.",
        "",
        "PR metadata:",
        json.dumps(summary, indent=2),
        "",
        "PR patch:",
        diff,
    ]
    if comments:
        body.extend(["", "Existing PR reviews/comments:", comments])
    if file_context:
        body.extend(["", "Current full contents of touched files:", file_context])
    body.extend(["", f"Review prompt: {args.prompt}"])

    return enforce_context_budget(
        ReviewContext(mode="review-pr", prompt=args.prompt, body="\n".join(body), skipped=skipped),
        config,
    )


def system_prompt(mode: str) -> str:
    base = (
        "You are a senior code reviewer in a multi-model review swarm. "
        "Analyze the provided context carefully. Be specific, cite files and lines "
        "when relevant, and focus on correctness, security, performance, maintainability, "
        "missing tests, and mismatches with the requested behavior. "
        "Do NOT implement fixes or produce patches; provide review feedback only."
    )
    if mode == "plan":
        return (
            "You are reviewing an architecture or implementation plan. "
            "Find design flaws, missing edge cases, risks, and tests to require. "
            "Do NOT implement fixes; provide review feedback only."
        )
    if mode == "review-pr":
        return base + " Treat the PR diff as the primary source of truth."
    return base


def call_openrouter(
    model: str,
    context: ReviewContext,
    config: SwarmConfig,
    api_key: str,
) -> dict[str, Any]:
    started = time.monotonic()
    payload = {
        "model": model,
        "messages": [
            {"role": "system", "content": system_prompt(context.mode)},
            {"role": "user", "content": context.body},
        ],
        "temperature": config.temperature,
        "max_tokens": config.max_tokens,
    }
    body = json.dumps(payload).encode("utf-8")
    request = urllib.request.Request(
        OPENROUTER_URL,
        data=body,
        method="POST",
        headers={
            "Authorization": f"Bearer {api_key}",
            "Content-Type": "application/json",
            "HTTP-Referer": REFERER,
            "X-Title": TITLE,
        },
    )

    try:
        with urllib.request.urlopen(request, timeout=config.timeout_seconds) as response:
            raw = response.read().decode("utf-8", errors="replace")
    except urllib.error.HTTPError as exc:
        detail = exc.read().decode("utf-8", errors="replace")
        return {
            "model": model,
            "error": f"OpenRouter returned {exc.code}: {detail.strip()}",
            "latency_ms": int((time.monotonic() - started) * 1000),
        }
    except (urllib.error.URLError, TimeoutError, socket.timeout) as exc:
        return {
            "model": model,
            "error": f"request failed: {exc}",
            "latency_ms": int((time.monotonic() - started) * 1000),
        }
    except Exception as exc:
        return {
            "model": model,
            "error": f"unexpected worker failure: {type(exc).__name__}",
            "latency_ms": int((time.monotonic() - started) * 1000),
        }

    try:
        data = json.loads(raw)
    except json.JSONDecodeError:
        return {
            "model": model,
            "error": f"invalid JSON response: {raw[:200]}",
            "latency_ms": int((time.monotonic() - started) * 1000),
        }

    choice = (data.get("choices") or [{}])[0]
    finish_reason = choice.get("finish_reason") or choice.get("native_finish_reason")
    message = (choice.get("message") or {})
    output = extract_message_text(message)
    if not output:
        error = (data.get("error") or {}).get("message") or "empty assistant response"
        return {
            "model": model,
            "error": f"API Error: {error}",
            "latency_ms": int((time.monotonic() - started) * 1000),
        }

    usage = data.get("usage") or {}
    return {
        "model": model,
        "output": output,
        "tokens": usage.get("total_tokens", 0),
        "finish_reason": finish_reason,
        "latency_ms": int((time.monotonic() - started) * 1000),
    }


def extract_content_text(value: Any, depth: int = 0) -> str:
    if depth > 10:
        return ""
    if isinstance(value, str):
        return value
    if isinstance(value, list):
        parts: list[str] = []
        for item in value:
            if isinstance(item, str):
                parts.append(item)
            elif isinstance(item, dict):
                for key in ("text", "content", "reasoning"):
                    text = extract_content_text(item.get(key), depth + 1)
                    if text:
                        parts.append(text)
                        break
        return "\n".join(part for part in parts if part)
    if isinstance(value, dict):
        for key in ("text", "content", "reasoning"):
            text = extract_content_text(value.get(key), depth + 1)
            if text:
                return text
    return ""


def extract_message_text(message: dict[str, Any]) -> str:
    for key in ("content", "reasoning"):
        text = extract_content_text(message.get(key))
        if text:
            return text
    return ""


def run_swarm(context: ReviewContext, config: SwarmConfig) -> dict[str, Any]:
    api_key = load_openrouter_key()
    started = time.monotonic()
    results: list[dict[str, Any]] = []
    errors: list[dict[str, Any]] = []

    with concurrent.futures.ThreadPoolExecutor(max_workers=len(config.models)) as pool:
        futures = {
            pool.submit(call_openrouter, model, context, config, api_key): model
            for model in config.models
        }
        for future in concurrent.futures.as_completed(futures):
            model = futures[future]
            try:
                item = future.result()
            except Exception as exc:
                item = {
                    "model": model,
                    "error": f"unexpected worker failure: {type(exc).__name__}: {exc}",
                    "latency_ms": 0,
                }
            if item.get("error"):
                errors.append(item)
            else:
                results.append(item)

    order = {model: index for index, model in enumerate(config.models)}
    results.sort(key=lambda item: order.get(item["model"], 999))
    errors.sort(key=lambda item: order.get(item["model"], 999))

    return {
        "mode": context.mode,
        "results": results,
        "errors": errors,
        "total_models": len(config.models),
        "successful": len(results),
        "failed": len(errors),
        "skipped": context.skipped,
        "total_latency_ms": int((time.monotonic() - started) * 1000),
        "created_at": dt.datetime.now(dt.UTC).isoformat(),
    }


def format_markdown(report: dict[str, Any]) -> str:
    lines = [
        f"# Codex Swarm: {report['mode']}",
        "",
        (
            f"Swarm complete: {report['successful']}/{report['total_models']} models, "
            f"{report['total_latency_ms']}ms total"
        ),
        "",
    ]

    if report.get("skipped"):
        lines.append("## Skipped / Truncated Context")
        lines.extend(f"- {item}" for item in report["skipped"])
        lines.append("")

    for result in report.get("results", []):
        finish = result.get("finish_reason") or "unknown"
        lines.append(
            f"## {result['model']} ({result['latency_ms']}ms, {result.get('tokens', 0)} tok, finish={finish})"
        )
        lines.append("")
        if finish not in ("stop", "end_turn", "complete", "unknown"):
            lines.append(f"> Reviewer finished with `{finish}`; output may be incomplete.")
            lines.append("")
        lines.append(result["output"].rstrip())
        lines.append("")

    for error in report.get("errors", []):
        lines.append(f"## {error['model']} (ERROR)")
        lines.append("")
        lines.append(error["error"])
        lines.append("")

    return "\n".join(lines).rstrip() + "\n"


def write_output(text: str, path: str | None) -> None:
    if not path:
        print(text, end="")
        return
    out = pathlib.Path(path)
    out.parent.mkdir(parents=True, exist_ok=True)
    out.write_text(text, encoding="utf-8")
    print(f"Wrote swarm report to {out}")


def helper_identity() -> dict[str, str]:
    return {
        "extension_id": HELPER_EXTENSION_ID,
        "extension_name": HELPER_EXTENSION_NAME,
        "extension_version": HELPER_EXTENSION_VERSION,
    }


def helper_capability(capability_type: str, local_id: str) -> dict[str, str]:
    identity = helper_identity()
    return {
        **identity,
        "capability_id": f"{identity['extension_id']}.{capability_type}.{local_id}",
        "capability_type": capability_type,
    }


def helper_sdk_snapshot() -> dict[str, Any]:
    review = helper_capability("tool", "review")
    return {
        "tools": [
            {
                "identity": review,
                "name": "swarm_review",
                "description": "Review bounded code or plan context with multiple reviewer models.",
                "params": [
                    {
                        "name": "prompt",
                        "description": "Review focus.",
                        "param_type": "string",
                        "required": True,
                    },
                    {
                        "name": "mode",
                        "description": "Review mode: plan, review-code, review-diff, or review-pr.",
                        "param_type": "string",
                        "required": False,
                    },
                    {
                        "name": "context",
                        "description": "Bounded explicit context for helper-driven reviews.",
                        "param_type": "string",
                        "required": False,
                    },
                    {
                        "name": "files",
                        "description": "Files to include when mode is review-code.",
                        "param_type": "string[]",
                        "required": False,
                    },
                    {
                        "name": "base",
                        "description": "Git base ref for review-diff.",
                        "param_type": "string",
                        "required": False,
                    },
                    {
                        "name": "staged",
                        "description": "Review the staged git diff.",
                        "param_type": "boolean",
                        "required": False,
                    },
                    {
                        "name": "pr",
                        "description": "PR number, URL, or branch understood by gh.",
                        "param_type": "string",
                        "required": False,
                    },
                    {
                        "name": "current",
                        "description": "Use the PR for the current branch.",
                        "param_type": "boolean",
                        "required": False,
                    },
                ],
                "required_capabilities": [],
                "limits": {
                    "timeout_ms": 120000,
                    "max_output_bytes": 32000,
                    "max_artifact_bytes": HELPER_MAX_ARTIFACT_BYTES,
                    "max_artifacts": 1,
                    "max_host_calls": 0,
                },
                "artifacts": [
                    {
                        "name": "report",
                        "description": "Markdown swarm review report.",
                        "artifact_type": "text/markdown",
                        "required": True,
                    }
                ],
                "golden_cases": [],
            }
        ],
        "slash_commands": [
            {
                "identity": helper_capability("slash_command", "code-swarm"),
                "surface_id": "/code-swarm",
                "display_name": "Code Swarm",
                "description": "Run or inspect Code Swarm reviews.",
                "visibility": "user",
            }
        ],
        "sidebar_panes": [
            {
                "identity": helper_capability("sidebar_pane", "status"),
                "surface_id": "code-swarm",
                "display_name": "Code Swarm",
                "description": "Show recent Code Swarm run status.",
                "visibility": "ui",
            }
        ],
        "lifecycle": [{"extension": helper_identity(), "state": "registered", "reason": None}],
    }


def helper_emit(message_id: str, message: dict[str, Any]) -> None:
    sys.stdout.write(
        json.dumps(
            {
                "host_abi_version": HOST_ABI_VERSION,
                "message_id": message_id,
                "message": message,
            },
            separators=(",", ":"),
        )
        + "\n"
    )
    sys.stdout.flush()


def helper_result(message_id: str, ok: bool, payload: dict[str, Any]) -> None:
    helper_emit(message_id, {"type": "result", "ok": ok, "payload": payload})


def helper_load_key(arguments: dict[str, Any]) -> str:
    env_name = str(arguments.get("key_env") or "OPENROUTER_API_KEY")
    key = os.environ.get(env_name, "").strip()
    if key:
        return key
    if env_name != "OPENROUTER_API_KEY":
        raise MissingKeyError(f"{env_name} is not set")
    try:
        return load_openrouter_key()
    except SystemExit as exc:
        raise MissingKeyError(str(exc)) from exc


def helper_report_path(arguments: dict[str, Any]) -> pathlib.Path:
    tmp_root = pathlib.Path("/tmp").resolve()
    explicit = str(arguments.get("out") or "").strip()
    if explicit:
        path = pathlib.Path(explicit)
        if not path.is_absolute():
            path = tmp_root / path
    else:
        out_dir = pathlib.Path(str(arguments.get("out_dir") or "/tmp"))
        if not out_dir.is_absolute():
            out_dir = tmp_root / out_dir
        out_dir = out_dir.resolve(strict=False)
        if out_dir != tmp_root:
            raise RuntimeError("Code Swarm helper reports must be written directly under /tmp")
        path = out_dir / f"euler-code-swarm-{uuid.uuid4().hex}.md"
    if path.is_symlink():
        raise RuntimeError("Code Swarm helper report path must not be a symlink")
    path = path.resolve(strict=False)
    if path.parent != tmp_root:
        raise RuntimeError("Code Swarm helper reports must be written directly under /tmp")
    return path


def helper_write_text(path: pathlib.Path, content: str) -> None:
    flags = os.O_WRONLY | os.O_CREAT | os.O_EXCL
    if hasattr(os, "O_NOFOLLOW"):
        flags |= os.O_NOFOLLOW
    fd = os.open(path, flags, 0o600)
    try:
        handle = os.fdopen(fd, "w", encoding="utf-8")
        fd = -1
        with handle:
            handle.write(content)
    finally:
        if fd != -1:
            os.close(fd)


def helper_is_review_capability(message: dict[str, Any]) -> bool:
    capability = message.get("capability") or {}
    return (
        capability.get("extension_id") == HELPER_EXTENSION_ID
        and capability.get("capability_id") == f"{HELPER_EXTENSION_ID}.tool.review"
        and capability.get("capability_type") == "tool"
    )


def helper_bool(arguments: dict[str, Any], name: str, default: bool = False) -> bool:
    value = arguments.get(name, default)
    if isinstance(value, bool):
        return value
    if value is None:
        return default
    raise ValueError(f"{name} must be boolean")


def helper_fake_report(arguments: dict[str, Any]) -> dict[str, Any]:
    results = list(arguments.get("fake_results") or [])
    errors = list(arguments.get("fake_errors") or [])
    report = {
        "mode": str(arguments.get("mode") or "review-diff"),
        "results": [
            {
                "model": str(item.get("model", "fake/model")),
                "output": str(item.get("output", "")),
                "tokens": int(item.get("tokens", 0)),
                "latency_ms": int(item.get("latency_ms", 0)),
            }
            for item in results
        ],
        "errors": [
            {
                "model": str(item.get("model", "fake/model")),
                "error": str(item.get("error", "model failure")),
                "latency_ms": int(item.get("latency_ms", 0)),
            }
            for item in errors
        ],
        "total_models": len(results) + len(errors),
        "successful": len(results),
        "failed": len(errors),
        "skipped": list(arguments.get("skipped") or []),
        "total_latency_ms": int(arguments.get("total_latency_ms", 0)),
        "created_at": dt.datetime.now(dt.UTC).isoformat(),
    }
    if report["total_models"] == 0:
        report["total_models"] = 1
        report["failed"] = 1
        report["errors"].append(
            {"model": "fake/model", "error": "no fake results configured", "latency_ms": 0}
        )
    return report


def helper_with_extra_context(
    context: ReviewContext,
    explicit_context: Any,
    config: SwarmConfig,
) -> ReviewContext:
    if explicit_context is None:
        return context
    return enforce_context_budget(
        ReviewContext(
            mode=context.mode,
            prompt=context.prompt,
            body=f"{context.body}\n\nAdditional caller context:\n\n{explicit_context}",
            skipped=context.skipped,
        ),
        config,
    )


def helper_review_context(arguments: dict[str, Any], config: SwarmConfig) -> ReviewContext:
    mode = str(arguments.get("mode") or "plan")
    prompt = str(arguments.get("prompt") or "Review for correctness.")
    explicit_context = arguments.get("context")

    if mode == "plan":
        return helper_with_extra_context(
            build_plan_context(prompt, config),
            explicit_context,
            config,
        )

    if mode == "review-code":
        files = arguments.get("files") or []
        if not isinstance(files, list) or not all(isinstance(item, str) for item in files):
            raise ValueError("files must be an array of strings")
        if not files:
            raise ValueError("review-code requires files")
        return helper_with_extra_context(
            build_code_context(argparse.Namespace(files=files, prompt=prompt), config),
            explicit_context,
            config,
        )

    if mode == "review-diff":
        return helper_with_extra_context(
            build_diff_context(
                argparse.Namespace(
                    prompt=prompt,
                    base=str(arguments.get("base") or "") or None,
                    staged=helper_bool(arguments, "staged"),
                ),
                config,
            ),
            explicit_context,
            config,
        )

    if mode == "review-pr":
        pr = str(arguments.get("pr") or "").strip()
        current = helper_bool(arguments, "current")
        if not pr and not current:
            raise ValueError("review-pr requires pr or current=true")
        max_diff_bytes = int(arguments.get("max_diff_bytes") or 500_000)
        if max_diff_bytes <= 0:
            raise ValueError("max_diff_bytes must be positive")
        return helper_with_extra_context(
            build_pr_context(
                argparse.Namespace(
                    prompt=prompt,
                    pr=pr or None,
                    current=current,
                    include_full_files=helper_bool(arguments, "include_full_files"),
                    include_comments=helper_bool(arguments, "include_comments"),
                    max_diff_bytes=max_diff_bytes,
                ),
                config,
            ),
            explicit_context,
            config,
        )

    if explicit_context is not None:
        return enforce_context_budget(
            ReviewContext(
                mode=mode,
                prompt=prompt,
                body=f"Review context:\n\n{explicit_context}\n\nReview prompt: {prompt}",
                skipped=[],
            ),
            config,
        )

    raise ValueError(f"unsupported review mode: {mode}")


def helper_real_report(arguments: dict[str, Any]) -> dict[str, Any]:
    config = load_config(arguments.get("config"))
    context = helper_review_context(arguments, config)
    api_key = helper_load_key(arguments)
    previous_key = os.environ.get("OPENROUTER_API_KEY")
    os.environ["OPENROUTER_API_KEY"] = api_key
    try:
        return run_swarm(context, config)
    finally:
        if previous_key is None:
            os.environ.pop("OPENROUTER_API_KEY", None)
        else:
            os.environ["OPENROUTER_API_KEY"] = previous_key


def helper_handle_call(message_id: str, arguments: dict[str, Any]) -> None:
    action = str(arguments.get("action") or "review")
    if action == "slow":
        try:
            test_mode = helper_bool(arguments, "test_mode")
        except ValueError as exc:
            helper_result(
                message_id,
                False,
                {"error": str(exc), "type": "ValueError", "code": "invalid_argument"},
            )
            return
        if not test_mode:
            helper_result(message_id, False, {"error": "slow action requires test_mode"})
            return
        time.sleep(min(max(float(arguments.get("seconds", 0.25)), 0.0), 1.0))
        helper_result(message_id, True, {"status": "slow complete"})
        return
    if action != "review":
        helper_result(message_id, False, {"error": "unknown action", "action": action})
        return

    try:
        max_output_bytes = int(arguments.get("max_output_bytes") or 32000)
        if max_output_bytes <= 0:
            helper_result(message_id, False, {"error": "max_output_bytes must be positive"})
            return
        fake_requested = (
            arguments.get("fake_results") is not None or arguments.get("fake_errors") is not None
        )
        if fake_requested and not helper_bool(arguments, "test_mode"):
            helper_result(message_id, False, {"error": "fake review requires test_mode"})
            return
        if fake_requested:
            report = helper_fake_report(arguments)
        else:
            report = helper_real_report(arguments)
        markdown = format_markdown(report)
        if utf8_len(markdown) > HELPER_MAX_ARTIFACT_BYTES:
            helper_result(message_id, False, {"error": "report exceeds max_artifact_bytes"})
            return
        path = helper_report_path(arguments)
        helper_write_text(path, markdown)
        ok = report.get("successful", 0) > 0
        content = markdown
        truncated = False
        if utf8_len(content) > max_output_bytes:
            content = content.encode("utf-8")[:max_output_bytes].decode("utf-8", errors="ignore")
            truncated = True
        helper_result(
            message_id,
            bool(ok),
            {
                "report_path": str(path),
                "content": content,
                "truncated": truncated,
                "successful": report.get("successful", 0),
                "failed": report.get("failed", 0),
                "errors": report.get("errors", []),
                "artifacts": [
                    {
                        "name": "report",
                        "artifact_type": "text/markdown",
                        "path": str(path),
                        "bytes": utf8_len(markdown),
                    }
                ],
            },
        )
    except MissingKeyError as exc:
        helper_result(
            message_id,
            False,
            {"error": str(exc), "type": "MissingKeyError", "code": "missing_api_key"},
        )
    except ValueError as exc:
        helper_result(
            message_id,
            False,
            {"error": str(exc), "type": "ValueError", "code": "invalid_argument"},
        )
    except Exception as exc:
        helper_result(
            message_id,
            False,
            {"error": str(exc), "type": type(exc).__name__, "code": "helper_error"},
        )


def run_process_helper() -> int:
    for raw in sys.stdin:
        if not raw.strip():
            continue
        try:
            envelope = json.loads(raw)
        except json.JSONDecodeError as exc:
            helper_result("invalid-json", False, {"error": f"invalid JSON: {exc}"})
            continue
        message_id = str(envelope.get("message_id") or "missing-id")
        if envelope.get("host_abi_version") != HOST_ABI_VERSION:
            helper_result(
                message_id,
                False,
                {
                    "error": "unsupported host ABI",
                    "expected": HOST_ABI_VERSION,
                    "actual": envelope.get("host_abi_version"),
                },
            )
            continue
        message = envelope.get("message") or {}
        message_type = message.get("type")
        if message_type == "init":
            helper_emit(message_id, {"type": "register_sdk", "snapshot": helper_sdk_snapshot()})
        elif message_type == "cancel":
            helper_result(
                message_id,
                False,
                {
                    "error": "cancellation is host-timeout based until async helper jobs are wired",
                    "cancellation_token": message.get("cancellation_token"),
                },
            )
        elif message_type == "call":
            if not helper_is_review_capability(message):
                helper_result(message_id, False, {"error": "unsupported capability"})
                continue
            helper_handle_call(message_id, dict(message.get("arguments") or {}))
        else:
            helper_result(message_id, False, {"error": "unsupported message", "type": message_type})
    return 0


def self_test_expect(condition: bool, message: str) -> None:
    if not condition:
        raise RuntimeError(f"helper self-test failed: {message}")


def helper_self_test_env(home: pathlib.Path) -> dict[str, str]:
    env: dict[str, str] = {}
    for key in ["PATH", "LANG", "LC_ALL", "TZ", "SYSTEMROOT", "WINDIR"]:
        if key in os.environ:
            env[key] = os.environ[key]
    env["HOME"] = str(home)
    env["USERPROFILE"] = str(home)
    env["PYTHONUNBUFFERED"] = "1"
    return env


def helper_self_test_envelope(message_id: str, message: dict[str, Any]) -> str:
    return json.dumps(
        {
            "host_abi_version": HOST_ABI_VERSION,
            "message_id": message_id,
            "message": message,
        },
        separators=(",", ":"),
    )


def helper_self_test_call(
    message_id: str,
    capability: dict[str, str],
    arguments: dict[str, Any],
) -> str:
    return helper_self_test_envelope(
        message_id,
        {
            "type": "call",
            "capability": capability,
            "arguments": arguments,
        },
    )


def helper_self_test_fake_args(
    out: pathlib.Path | str,
    *,
    test_mode: bool | None = True,
) -> dict[str, Any]:
    arguments: dict[str, Any] = {
        "action": "review",
        "mode": "plan",
        "prompt": "Review deterministic helper self-test context.",
        "context": "self-test context",
        "out": str(out),
        "max_output_bytes": HELPER_SELF_TEST_MAX_OUTPUT_BYTES,
        "fake_results": [
            {
                "model": "fake/fable",
                "output": "deterministic fake review",
                "tokens": 7,
                "latency_ms": 3,
            }
        ],
    }
    if test_mode is not None:
        arguments["test_mode"] = test_mode
    return arguments


def parse_helper_self_test_line(line: str) -> tuple[str, dict[str, Any]]:
    self_test_expect(
        utf8_len(line) <= HELPER_SELF_TEST_MAX_PROTOCOL_LINE_BYTES,
        "protocol line exceeded helper self-test byte limit",
    )
    parsed = json.loads(line)
    self_test_expect(isinstance(parsed, dict), "response line must be an object")
    self_test_expect(
        parsed.get("host_abi_version") == HOST_ABI_VERSION,
        "response host ABI mismatch",
    )
    message_id = str(parsed.get("message_id") or "")
    self_test_expect(message_id, "response missing message_id")
    message = parsed.get("message")
    self_test_expect(isinstance(message, dict), f"response {message_id} missing message")
    return message_id, message


def read_helper_self_test_response(
    proc: subprocess.Popen[bytes],
    selector: selectors.BaseSelector,
    stdout_buffer: bytearray,
) -> tuple[str, dict[str, Any]]:
    self_test_expect(proc.stdout is not None, "helper stdout pipe missing")
    deadline = time.monotonic() + HELPER_SELF_TEST_TIMEOUT_SECONDS
    while True:
        newline = stdout_buffer.find(b"\n")
        if newline != -1:
            line = bytes(stdout_buffer[:newline])
            del stdout_buffer[: newline + 1]
            return parse_helper_self_test_line(line.decode("utf-8").rstrip("\n"))
        self_test_expect(
            len(stdout_buffer) <= HELPER_SELF_TEST_MAX_PROTOCOL_LINE_BYTES,
            "protocol line exceeded helper self-test byte limit",
        )
        remaining = deadline - time.monotonic()
        self_test_expect(remaining > 0, "timed out waiting for helper response")
        ready = selector.select(timeout=remaining)
        self_test_expect(bool(ready), "timed out waiting for helper response")
        try:
            chunk = os.read(proc.stdout.fileno(), 65536)
        except BlockingIOError:
            continue
        self_test_expect(chunk != b"", "helper exited before response")
        stdout_buffer.extend(chunk)


def send_helper_self_test_line(
    proc: subprocess.Popen[bytes],
    selector: selectors.BaseSelector,
    stdout_buffer: bytearray,
    line: str,
    messages: dict[str, dict[str, Any]],
) -> None:
    self_test_expect(proc.stdin is not None, "helper stdin pipe missing")
    proc.stdin.write(f"{line}\n".encode("utf-8"))
    proc.stdin.flush()
    message_id, message = read_helper_self_test_response(proc, selector, stdout_buffer)
    self_test_expect(message_id not in messages, f"duplicate response id {message_id}")
    messages[message_id] = message


def drain_helper_self_test_stdout(
    proc: subprocess.Popen[bytes],
    selector: selectors.BaseSelector,
    stdout_buffer: bytearray,
) -> str:
    extra = bytearray(stdout_buffer)
    stdout_buffer.clear()
    if proc.stdout is None:
        return extra.decode("utf-8", errors="replace")
    deadline = time.monotonic() + HELPER_SELF_TEST_TIMEOUT_SECONDS
    while True:
        try:
            chunk = os.read(proc.stdout.fileno(), 65536)
        except BlockingIOError:
            remaining = deadline - time.monotonic()
            self_test_expect(remaining > 0, "timed out draining helper stdout")
            selector.select(timeout=min(remaining, 0.1))
            continue
        if not chunk:
            break
        extra.extend(chunk)
    return extra.decode("utf-8", errors="replace")


def finish_helper_self_test_process(
    proc: subprocess.Popen[bytes],
    selector: selectors.BaseSelector,
    stdout_buffer: bytearray,
) -> str:
    if proc.stdin is not None:
        proc.stdin.close()
    try:
        proc.wait(timeout=HELPER_SELF_TEST_TIMEOUT_SECONDS)
    except subprocess.TimeoutExpired as exc:
        proc.kill()
        proc.wait()
        raise RuntimeError("helper self-test timed out during shutdown") from exc
    extra = drain_helper_self_test_stdout(proc, selector, stdout_buffer)
    self_test_expect(extra == "", f"helper emitted unexpected extra stdout: {extra}")
    self_test_expect(proc.stderr is not None, "helper stderr pipe missing")
    return proc.stderr.read().decode("utf-8", errors="replace")


def helper_self_test_payload(messages: dict[str, dict[str, Any]], message_id: str) -> dict[str, Any]:
    message = messages.get(message_id)
    self_test_expect(message is not None, f"missing response {message_id}")
    self_test_expect(message.get("type") == "result", f"{message_id} did not return a result")
    payload = message.get("payload")
    self_test_expect(isinstance(payload, dict), f"{message_id} missing payload object")
    return payload


def assert_helper_self_test_error(
    messages: dict[str, dict[str, Any]],
    message_id: str,
    expected_fragment: str,
) -> None:
    message = messages.get(message_id)
    self_test_expect(message is not None, f"missing response {message_id}")
    self_test_expect(message.get("type") == "result", f"{message_id} did not return a result")
    self_test_expect(message.get("ok") is False, f"{message_id} unexpectedly succeeded")
    payload = message.get("payload")
    self_test_expect(isinstance(payload, dict), f"{message_id} missing error payload")
    error = str(payload.get("error") or "")
    self_test_expect(
        expected_fragment in error,
        f"{message_id} error `{error}` did not contain `{expected_fragment}`",
    )
    self_test_expect("artifacts" not in payload, f"{message_id} unexpectedly reported artifacts")


def assert_helper_self_test_snapshot(messages: dict[str, dict[str, Any]]) -> None:
    message = messages.get("init-1")
    self_test_expect(message is not None, "missing init response")
    self_test_expect(message.get("type") == "register_sdk", "init did not register SDK")
    snapshot = message.get("snapshot")
    self_test_expect(isinstance(snapshot, dict), "init snapshot missing")
    tools = snapshot.get("tools")
    self_test_expect(isinstance(tools, list), "snapshot tools missing")
    review_tools = [
        tool for tool in tools if isinstance(tool, dict) and tool.get("name") == "swarm_review"
    ]
    self_test_expect(len(review_tools) == 1, "snapshot must expose one swarm_review tool")
    review = review_tools[0]
    identity = review.get("identity") or {}
    self_test_expect(
        identity.get("extension_id") == HELPER_EXTENSION_ID,
        "review tool extension id mismatch",
    )
    self_test_expect(
        identity.get("capability_id") == f"{HELPER_EXTENSION_ID}.tool.review",
        "review tool capability id mismatch",
    )
    self_test_expect(
        identity.get("capability_type") == "tool",
        "review tool capability type mismatch",
    )
    limits = review.get("limits") or {}
    self_test_expect(limits.get("max_artifacts") == 1, "review tool must report one artifact max")
    self_test_expect(
        limits.get("max_artifact_bytes") == HELPER_MAX_ARTIFACT_BYTES,
        "review tool artifact byte limit mismatch",
    )


def assert_helper_self_test_success(
    messages: dict[str, dict[str, Any]],
    report_path: pathlib.Path,
    tmp_root: pathlib.Path,
) -> None:
    message = messages.get("review-ok")
    self_test_expect(message is not None, "missing review-ok response")
    self_test_expect(message.get("type") == "result", "review-ok did not return a result")
    self_test_expect(message.get("ok") is True, "review-ok did not succeed")
    payload = helper_self_test_payload(messages, "review-ok")
    self_test_expect(payload.get("successful") == 1, "review-ok successful count mismatch")
    self_test_expect(payload.get("failed") == 0, "review-ok failed count mismatch")
    self_test_expect(payload.get("report_path") == str(report_path), "report path mismatch")
    content = str(payload.get("content") or "")
    self_test_expect("deterministic fake review" in content, "fake review content missing")
    self_test_expect(
        utf8_len(content) <= HELPER_SELF_TEST_MAX_OUTPUT_BYTES,
        "returned content exceeded helper self-test output bound",
    )
    artifacts = payload.get("artifacts")
    self_test_expect(isinstance(artifacts, list) and len(artifacts) == 1, "expected one artifact")
    artifact = artifacts[0]
    self_test_expect(artifact.get("path") == str(report_path), "artifact path mismatch")
    self_test_expect(
        artifact.get("artifact_type") == "text/markdown",
        "artifact type mismatch",
    )
    self_test_expect(report_path.parent == tmp_root, "report path is not under /tmp")
    self_test_expect(report_path.exists(), "report artifact missing")
    report_bytes = report_path.read_bytes()
    self_test_expect(
        len(report_bytes) <= HELPER_MAX_ARTIFACT_BYTES,
        "report artifact exceeded helper artifact byte limit",
    )
    if os.name == "posix":
        self_test_expect(
            report_path.stat().st_mode & 0o777 == 0o600,
            "report artifact mode must be 0600",
        )


def run_helper_self_test() -> int:
    self_test_expect(os.name == "posix", "helper self-test requires POSIX")
    self_test_expect(pathlib.Path("/tmp").is_dir(), "helper self-test requires writable /tmp")
    tmp_root = pathlib.Path("/tmp").resolve()
    unique = uuid.uuid4().hex
    report_path = tmp_root / f"euler-code-swarm-self-test-{unique}.md"
    no_test_path = tmp_root / f"euler-code-swarm-self-test-no-test-{unique}.md"
    bad_cap_path = tmp_root / f"euler-code-swarm-self-test-bad-cap-{unique}.md"
    nested_path = tmp_root / f"euler-code-swarm-self-test-{unique}" / "report.md"
    traversal_path = pathlib.Path(f"/tmp/../etc/euler-code-swarm-self-test-{unique}.md")
    symlink_path = tmp_root / f"euler-code-swarm-self-test-link-{unique}.md"
    symlink_target = pathlib.Path("/etc") / f"euler-code-swarm-self-test-target-{unique}.md"
    cleanup_paths = [
        report_path,
        no_test_path,
        bad_cap_path,
        nested_path,
        symlink_path,
    ]
    review_capability = helper_capability("tool", "review")
    bad_capability = helper_capability("tool", "unsupported")

    try:
        symlink_path.symlink_to(symlink_target)
        include_symlink_case = True
    except OSError:
        include_symlink_case = False

    lines = [
        helper_self_test_envelope("init-1", {"type": "init"}),
        helper_self_test_call(
            "review-ok",
            review_capability,
            helper_self_test_fake_args(report_path),
        ),
        helper_self_test_call(
            "fake-without-test-mode",
            review_capability,
            helper_self_test_fake_args(no_test_path, test_mode=None),
        ),
        helper_self_test_call(
            "unsupported-capability",
            bad_capability,
            helper_self_test_fake_args(bad_cap_path),
        ),
        helper_self_test_call(
            "unsafe-nested-path",
            review_capability,
            helper_self_test_fake_args(nested_path),
        ),
        helper_self_test_call(
            "unsafe-traversal-path",
            review_capability,
            helper_self_test_fake_args(str(traversal_path)),
        ),
        "{not-json",
    ]
    if include_symlink_case:
        lines.insert(
            -1,
            helper_self_test_call(
                "unsafe-symlink-path",
                review_capability,
                helper_self_test_fake_args(symlink_path),
            ),
        )

    proc: subprocess.Popen[bytes] | None = None
    selector: selectors.BaseSelector | None = None
    stdout_buffer = bytearray()
    messages: dict[str, dict[str, Any]] = {}
    try:
        with tempfile.TemporaryDirectory(prefix="euler-codeswarm-helper-home-") as home:
            proc = subprocess.Popen(
                [sys.executable, str(pathlib.Path(__file__).resolve()), "helper"],
                stdin=subprocess.PIPE,
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
                env=helper_self_test_env(pathlib.Path(home)),
                cwd=home,
            )
            selector = selectors.DefaultSelector()
            self_test_expect(proc.stdout is not None, "helper stdout pipe missing")
            os.set_blocking(proc.stdout.fileno(), False)
            selector.register(proc.stdout, selectors.EVENT_READ)
            for line in lines:
                send_helper_self_test_line(proc, selector, stdout_buffer, line, messages)
            stderr = finish_helper_self_test_process(proc, selector, stdout_buffer)
            selector.close()
            selector = None
            self_test_expect(proc.returncode == 0, f"helper exited with {proc.returncode}")
            self_test_expect(stderr == "", f"helper wrote stderr: {stderr}")
            expected_ids = {
                "init-1",
                "review-ok",
                "fake-without-test-mode",
                "unsupported-capability",
                "unsafe-nested-path",
                "unsafe-traversal-path",
                "invalid-json",
            }
            if include_symlink_case:
                expected_ids.add("unsafe-symlink-path")
            self_test_expect(set(messages) == expected_ids, "unexpected helper response ids")
            assert_helper_self_test_snapshot(messages)
            assert_helper_self_test_success(messages, report_path, tmp_root)
            assert_helper_self_test_error(
                messages,
                "fake-without-test-mode",
                "fake review requires test_mode",
            )
            assert_helper_self_test_error(
                messages,
                "unsupported-capability",
                "unsupported capability",
            )
            assert_helper_self_test_error(
                messages,
                "unsafe-nested-path",
                "directly under /tmp",
            )
            assert_helper_self_test_error(
                messages,
                "unsafe-traversal-path",
                "directly under /tmp",
            )
            if include_symlink_case:
                assert_helper_self_test_error(
                    messages,
                    "unsafe-symlink-path",
                    "must not be a symlink",
                )
            assert_helper_self_test_error(messages, "invalid-json", "invalid JSON")
            self_test_expect(
                not no_test_path.exists(),
                "non-test-mode failure artifact was created",
            )
            self_test_expect(
                not bad_cap_path.exists(),
                "bad-capability failure artifact was created",
            )
            self_test_expect(not nested_path.exists(), "nested unsafe artifact was created")
            self_test_expect(not traversal_path.exists(), "traversal unsafe artifact was created")
            self_test_expect(not symlink_target.exists(), "symlink target artifact was created")
    except Exception:
        if proc is not None and proc.poll() is None:
            proc.kill()
            proc.wait()
        raise
    finally:
        if selector is not None:
            selector.close()
        try:
            if nested_path.parent.exists():
                nested_path.parent.rmdir()
        except OSError:
            pass
        for path in cleanup_paths:
            try:
                path.unlink()
            except FileNotFoundError:
                pass

    self_test_expect(not report_path.exists(), "report artifact cleanup failed")
    print("codex_swarm helper self-test (transitional): passed")
    return 0


def add_common_args(parser: argparse.ArgumentParser) -> None:
    parser.add_argument("--prompt", required=True, help="Review question or focus.")
    parser.add_argument("--config", help="Path to JSON config. Defaults to .codex-swarm.json.")
    parser.add_argument("--json", action="store_true", help="Emit structured JSON instead of Markdown.")
    parser.add_argument("--out", help="Write output to this file instead of stdout.")
    parser.add_argument(
        "--dry-run",
        action="store_true",
        help="Print the assembled review context without calling OpenRouter.",
    )


def format_dry_run(context: ReviewContext) -> str:
    lines = [
        f"# Codex Swarm Dry Run: {context.mode}",
        "",
        f"Context bytes: {utf8_len(context.body)}",
        "",
    ]
    if context.skipped:
        lines.append("## Skipped / Truncated Context")
        lines.extend(f"- {item}" for item in context.skipped)
        lines.append("")
    lines.extend(["## Review Context", "", context.body])
    return "\n".join(lines).rstrip() + "\n"


def parse_args(argv: list[str]) -> argparse.Namespace:
    parser = argparse.ArgumentParser(description="Run a review-only Codex Code Swarm via OpenRouter.")
    sub = parser.add_subparsers(dest="command", required=True)

    plan = sub.add_parser("plan", help="Review a plan/design prompt.")
    add_common_args(plan)

    code = sub.add_parser("review-code", help="Review explicit local files.")
    add_common_args(code)
    code.add_argument("--files", nargs="+", required=True, help="Files to include.")

    diff = sub.add_parser("review-diff", help="Review a local git diff.")
    add_common_args(diff)
    diff.add_argument("--base", help="Review diff from BASE...HEAD.")
    diff.add_argument("--staged", action="store_true", help="Review staged changes.")

    pr = sub.add_parser("review-pr", help="Review a GitHub PR using gh.")
    add_common_args(pr)
    pr_target = pr.add_mutually_exclusive_group(required=True)
    pr_target.add_argument("--pr", help="PR number, URL, or branch understood by gh.")
    pr_target.add_argument("--current", action="store_true", help="Use the PR for the current branch.")
    pr.add_argument(
        "--include-full-files",
        action="store_true",
        help="Include current local contents for touched files.",
    )
    pr.add_argument(
        "--include-comments",
        action="store_true",
        help="Include existing PR reviews and comments.",
    )
    pr.add_argument(
        "--max-diff-bytes",
        type=int,
        default=500_000,
        help="Maximum PR diff bytes to send.",
    )

    models = sub.add_parser("models", help="Print configured models.")
    models.add_argument("--config", help="Path to JSON config. Defaults to .codex-swarm.json.")

    sub.add_parser("helper", help="Run as an Euler process-backed extension helper.")
    sub.add_parser(
        "helper-self-test",
        help="Run transitional no-network helper stdio checks; requires POSIX /tmp.",
    )

    return parser.parse_args(argv)


def main(argv: list[str]) -> int:
    args = parse_args(argv)
    if args.command == "helper":
        return run_process_helper()
    if args.command == "helper-self-test":
        return run_helper_self_test()
    config = load_config(getattr(args, "config", None))
    if getattr(args, "max_diff_bytes", 1) <= 0:
        raise SystemExit("--max-diff-bytes must be positive.")

    if args.command == "models":
        print(json.dumps(config.models, indent=2))
        return 0

    if args.command == "plan":
        context = build_plan_context(args.prompt, config)
    elif args.command == "review-code":
        context = build_code_context(args, config)
    elif args.command == "review-diff":
        context = build_diff_context(args, config)
    elif args.command == "review-pr":
        context = build_pr_context(args, config)
    else:
        raise SystemExit(f"unknown command: {args.command}")

    if args.dry_run:
        output = (
            json.dumps(dataclasses.asdict(context), indent=2) + "\n"
            if args.json
            else format_dry_run(context)
        )
        write_output(output, args.out)
        return 0

    report = run_swarm(context, config)
    output = json.dumps(report, indent=2) + "\n" if args.json else format_markdown(report)
    write_output(output, args.out)
    return 1 if report["successful"] == 0 else 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))
