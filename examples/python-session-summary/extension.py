"""Summarize a session using Euler's managed-process Python SDK."""

import json

from euler_managed_process_sdk import CommandContext, serve


def summarize(context: CommandContext) -> dict[str, object]:
    context.host.progress("reading provenance", 0.25)
    page = context.host.query_provenance(limit=256, scan_limit=1024)

    counts: dict[str, int] = {}
    for event in page.get("events", []):
        kind = event.get("kind", "unknown")
        counts[kind] = counts.get(kind, 0) + 1

    summary = {
        "event_count": len(page.get("events", [])),
        "event_kinds": counts,
        "truncated": page.get("truncated", False),
    }
    artifact = context.host.write_artifact(
        display_name="python-session-summary.json",
        media_type="application/json",
        data=(json.dumps(summary, indent=2) + "\n").encode("utf-8"),
        metadata={"producer": "python-session-summary"},
    )
    context.host.progress("summary written", 1.0)
    return {"summary": summary, "artifact": artifact}


serve({"summarize": summarize})
