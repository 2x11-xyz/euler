# Python session summary

This is a standalone Euler managed-process extension using `uv` for its Python
environment and dependency installation.

Euler managed-process execution, and this `.venv/bin/python` entrypoint, are
currently supported on macOS and Linux.

## Setup

Install [uv](https://docs.astral.sh/uv/) if needed, then run from this
 directory:

```sh
uv sync
```

The manifest points at `.venv/bin/python`, so Euler launches the environment
directly rather than relying on shell activation.

## Run

```sh
euler extension validate .
euler extension link .
euler extension enable python-session-summary
euler extension run python-session-summary.summarize /path/to/events.jsonl
```

The command returns event counts and writes `python-session-summary.json` as an
Euler artifact.
