"""A small proof package for local development from this repository.

For a standalone package, install the SDK into the interpreter named by the
manifest entrypoint instead of using this repository-relative development path.
"""

from pathlib import Path
import sys

SDK_SOURCE = Path(__file__).resolve().parents[2] / "python" / "euler_managed_process_sdk" / "src"
sys.path.insert(0, str(SDK_SOURCE))

from euler_managed_process_sdk import CommandContext, serve


def inspect(context: CommandContext) -> dict[str, object]:
    context.host.progress("reading a bounded provenance page", 0.25)
    page = context.host.query_provenance(limit=8, scan_limit=32)
    event_ids = [event["id"] for event in page["events"]]
    artifact = context.host.write_artifact(
        display_name="python-proof-summary.json",
        media_type="application/json",
        data=("{\"event_count\":%d}" % len(event_ids)).encode("utf-8"),
        source_event_ids=event_ids,
        metadata={"producer": "python-proof"},
    )
    context.host.progress("artifact written", 1.0)
    return {"event_count": len(event_ids), "artifact": artifact}


serve({"inspect": inspect})
