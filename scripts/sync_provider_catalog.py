#!/usr/bin/env python3
"""Synchronize Euler's embedded provider catalog from its GitHub release."""

from __future__ import annotations

import argparse
import dataclasses
import datetime as dt
import hashlib
import json
import os
from pathlib import Path
import re
import sys
import time
from typing import Optional
import urllib.error
import urllib.parse
import urllib.request


CATALOG_REPOSITORY = "2x11-xyz/euler-provider-catalog"
LATEST_MANIFEST_URL = (
    f"https://github.com/{CATALOG_REPOSITORY}/releases/latest/download/manifest-v1.json"
)
RELEASE_DOWNLOAD_ROOT = (
    f"https://github.com/{CATALOG_REPOSITORY}/releases/download/"
)
MANIFEST_NAME = "manifest-v1.json"
CATALOG_NAME = "catalog-v1.json"
MANIFEST_LIMIT_BYTES = 64 * 1024
ARTIFACT_LIMIT_BYTES = 16 * 1024 * 1024
MAX_REDIRECTS = 5
HTTP_DEADLINE_SECONDS = 30.0
ALLOWED_GITHUB_HOSTS = frozenset(
    {
        "github.com",
        "release-assets.githubusercontent.com",
        "objects.githubusercontent.com",
        "github-releases.githubusercontent.com",
    }
)
RELEASE_ID_PATTERN = re.compile(
    r"^catalog-v1-(?P<timestamp>[0-9]{8}t[0-9]{6}z)-(?P<digest>[0-9a-f]{64})$"
)
SHA256_PATTERN = re.compile(r"^[0-9a-f]{64}$")


class CatalogSyncError(Exception):
    """A safe, actionable catalog synchronization failure."""


@dataclasses.dataclass(frozen=True)
class ArtifactMetadata:
    byte_len: int
    sha256: str

    def identity_value(self) -> dict[str, object]:
        return {"bytes": self.byte_len, "sha256": self.sha256}


@dataclasses.dataclass(frozen=True)
class ReleaseManifest:
    schema_version: int
    release_id: str
    generated_at: str
    minimum_euler_version: str
    catalog: ArtifactMetadata
    provenance: ArtifactMetadata

    def identity_value(self) -> dict[str, object]:
        return {
            "artifacts": {
                CATALOG_NAME: self.catalog.identity_value(),
                "provenance-v1.json": self.provenance.identity_value(),
            },
            "generated_at": self.generated_at,
            "minimum_euler_version": self.minimum_euler_version,
            "schema_version": self.schema_version,
        }


@dataclasses.dataclass(frozen=True)
class CatalogRelease:
    manifest: ReleaseManifest
    manifest_bytes: bytes
    catalog_bytes: bytes


class _NoRedirect(urllib.request.HTTPRedirectHandler):
    def redirect_request(self, req, fp, code, msg, headers, newurl):  # noqa: ANN001
        return None


def _reject_duplicate_keys(pairs: list[tuple[str, object]]) -> dict[str, object]:
    result: dict[str, object] = {}
    for key, value in pairs:
        if key in result:
            raise CatalogSyncError(f"duplicate JSON key {key!r}")
        result[key] = value
    return result


def _reject_non_json_constant(value: str) -> object:
    raise CatalogSyncError(f"non-JSON numeric constant {value!r}")


def _strict_json_object(data: bytes, source: str) -> dict[str, object]:
    if data.startswith(b"\xef\xbb\xbf"):
        raise CatalogSyncError(f"{source} must not contain a UTF-8 byte-order mark")
    try:
        text = data.decode("utf-8")
    except UnicodeDecodeError as error:
        raise CatalogSyncError(f"{source} is not UTF-8") from error
    try:
        value = json.loads(
            text,
            object_pairs_hook=_reject_duplicate_keys,
            parse_constant=_reject_non_json_constant,
        )
    except CatalogSyncError:
        raise
    except (json.JSONDecodeError, ValueError) as error:
        raise CatalogSyncError(f"{source} is not strict JSON: {error}") from error
    if not isinstance(value, dict):
        raise CatalogSyncError(f"{source} must be a JSON object")
    return value


def _require_exact_keys(value: dict[str, object], expected: set[str], path: str) -> None:
    actual = set(value)
    if actual == expected:
        return
    missing = sorted(expected - actual)
    unknown = sorted(actual - expected)
    details = []
    if missing:
        details.append(f"missing {missing}")
    if unknown:
        details.append(f"unknown {unknown}")
    raise CatalogSyncError(f"{path} has invalid fields ({'; '.join(details)})")


def _require_string(value: object, path: str) -> str:
    if not isinstance(value, str) or not value:
        raise CatalogSyncError(f"{path} must be a non-empty string")
    return value


def _require_positive_integer(value: object, path: str) -> int:
    if type(value) is not int or value <= 0:  # bool is intentionally rejected.
        raise CatalogSyncError(f"{path} must be a positive integer")
    return value


def _parse_semver(value: str, path: str) -> tuple[int, int, int]:
    parts = value.split(".")
    invalid_part = any(
        not part or not part.isascii() or not part.isdigit() for part in parts
    )
    if len(parts) != 3 or invalid_part:
        raise CatalogSyncError(f"{path} must be a three-part numeric version")
    return (int(parts[0]), int(parts[1]), int(parts[2]))


def _parse_generated_at(value: str) -> dt.datetime:
    try:
        parsed = dt.datetime.strptime(value, "%Y-%m-%dT%H:%M:%SZ").replace(
            tzinfo=dt.timezone.utc
        )
    except ValueError as error:
        raise CatalogSyncError("manifest.generated_at is not canonical UTC seconds") from error
    if parsed.strftime("%Y-%m-%dT%H:%M:%SZ") != value:
        raise CatalogSyncError("manifest.generated_at is not canonical UTC seconds")
    return parsed


def _parse_artifact(value: object, path: str) -> ArtifactMetadata:
    if not isinstance(value, dict):
        raise CatalogSyncError(f"{path} must be an object")
    _require_exact_keys(value, {"bytes", "sha256"}, path)
    byte_len = _require_positive_integer(value["bytes"], f"{path}.bytes")
    if byte_len > ARTIFACT_LIMIT_BYTES:
        raise CatalogSyncError(f"{path}.bytes exceeds {ARTIFACT_LIMIT_BYTES}")
    sha256 = _require_string(value["sha256"], f"{path}.sha256")
    if SHA256_PATTERN.fullmatch(sha256) is None:
        raise CatalogSyncError(f"{path}.sha256 must be a lowercase SHA-256 digest")
    return ArtifactMetadata(byte_len=byte_len, sha256=sha256)


def canonical_manifest_identity(manifest: ReleaseManifest) -> bytes:
    # Protocol invariant shared with euler-provider-catalog's
    # catalog_pipeline.common.catalog_release_id and Euler's Rust consumer:
    # recursively sorted keys, two-space pretty JSON, and one trailing LF.
    encoded = json.dumps(manifest.identity_value(), indent=2, sort_keys=True)
    return f"{encoded}\n".encode("utf-8")


def expected_release_id(manifest: ReleaseManifest) -> str:
    generated_at = _parse_generated_at(manifest.generated_at)
    timestamp = generated_at.strftime("%Y%m%dt%H%M%Sz")
    digest = hashlib.sha256(canonical_manifest_identity(manifest)).hexdigest()
    return f"catalog-v1-{timestamp}-{digest}"


def parse_manifest(data: bytes) -> ReleaseManifest:
    if not data or len(data) > MANIFEST_LIMIT_BYTES:
        raise CatalogSyncError("manifest size is out of bounds")
    value = _strict_json_object(data, "provider catalog manifest")
    _require_exact_keys(
        value,
        {
            "artifacts",
            "generated_at",
            "minimum_euler_version",
            "release_id",
            "schema_version",
        },
        "manifest",
    )
    if type(value["schema_version"]) is not int or value["schema_version"] != 1:
        raise CatalogSyncError("manifest.schema_version must be 1")
    artifacts = value["artifacts"]
    if not isinstance(artifacts, dict):
        raise CatalogSyncError("manifest.artifacts must be an object")
    _require_exact_keys(
        artifacts, {CATALOG_NAME, "provenance-v1.json"}, "manifest.artifacts"
    )
    generated_at = _require_string(value["generated_at"], "manifest.generated_at")
    _parse_generated_at(generated_at)
    minimum = _require_string(
        value["minimum_euler_version"], "manifest.minimum_euler_version"
    )
    _parse_semver(minimum, "manifest.minimum_euler_version")
    release_id = _require_string(value["release_id"], "manifest.release_id")
    manifest = ReleaseManifest(
        schema_version=1,
        release_id=release_id,
        generated_at=generated_at,
        minimum_euler_version=minimum,
        catalog=_parse_artifact(
            artifacts[CATALOG_NAME], f"manifest.artifacts.{CATALOG_NAME}"
        ),
        provenance=_parse_artifact(
            artifacts["provenance-v1.json"],
            "manifest.artifacts.provenance-v1.json",
        ),
    )
    if RELEASE_ID_PATTERN.fullmatch(release_id) is None:
        raise CatalogSyncError("manifest.release_id has an invalid format")
    if release_id != expected_release_id(manifest):
        raise CatalogSyncError("manifest.release_id does not authenticate the manifest")
    return manifest


def validate_catalog(manifest: ReleaseManifest, data: bytes) -> None:
    if len(data) != manifest.catalog.byte_len:
        raise CatalogSyncError("catalog byte length does not match the manifest")
    if hashlib.sha256(data).hexdigest() != manifest.catalog.sha256:
        raise CatalogSyncError("catalog digest does not match the manifest")
    value = _strict_json_object(data, "provider catalog")
    _require_exact_keys(value, {"providers", "schema_version"}, "catalog")
    if type(value["schema_version"]) is not int or value["schema_version"] != 1:
        raise CatalogSyncError("catalog.schema_version must be 1")
    if not isinstance(value["providers"], dict) or not value["providers"]:
        raise CatalogSyncError("catalog.providers must be a non-empty object")


def validate_github_url(url: str) -> None:
    try:
        parsed = urllib.parse.urlsplit(url)
        port = parsed.port
    except ValueError as error:
        raise CatalogSyncError("provider catalog URL has an invalid port") from error
    if (
        parsed.scheme != "https"
        or parsed.hostname not in ALLOWED_GITHUB_HOSTS
        or parsed.username is not None
        or parsed.password is not None
        or port not in (None, 443)
        or parsed.fragment
    ):
        raise CatalogSyncError("provider catalog URL is outside the GitHub release boundary")


def _read_bounded(response, maximum: int, source: str) -> bytes:  # noqa: ANN001
    header = response.headers.get("Content-Length")
    declared_length: Optional[int] = None
    if header is not None:
        if not header.isascii() or not header.isdigit():
            raise CatalogSyncError(f"{source} Content-Length is invalid")
        declared_length = int(header)
        if declared_length > maximum:
            raise CatalogSyncError(f"{source} exceeds {maximum} bytes")
    data = response.read(maximum + 1)
    if len(data) > maximum:
        raise CatalogSyncError(f"{source} exceeds {maximum} bytes")
    if declared_length is not None and len(data) != declared_length:
        raise CatalogSyncError(f"{source} did not match Content-Length")
    return data


def _redirect_target(
    current: str, location: Optional[str], redirect_count: int
) -> str:
    if redirect_count >= MAX_REDIRECTS:
        raise CatalogSyncError("provider catalog redirect limit exceeded")
    if not location:
        raise CatalogSyncError("provider catalog redirect omitted Location")
    target = urllib.parse.urljoin(current, location)
    validate_github_url(target)
    return target


def fetch_github_asset(url: str, maximum: int) -> bytes:
    validate_github_url(url)
    opener = urllib.request.build_opener(_NoRedirect())
    deadline = time.monotonic() + HTTP_DEADLINE_SECONDS
    current = url
    for redirect_count in range(MAX_REDIRECTS + 1):
        remaining = deadline - time.monotonic()
        if remaining <= 0:
            raise CatalogSyncError("provider catalog request timed out")
        request = urllib.request.Request(
            current,
            headers={
                "User-Agent": "euler-provider-catalog-sync/1",
            },
        )
        try:
            response = opener.open(request, timeout=remaining)
        except urllib.error.HTTPError as error:
            if 300 <= error.code < 400:
                location = error.headers.get("Location")
                error.close()
                current = _redirect_target(current, location, redirect_count)
                continue
            status = error.code
            error.close()
            raise CatalogSyncError(
                f"provider catalog request returned HTTP {status}"
            ) from None
        except (urllib.error.URLError, TimeoutError, OSError) as error:
            reason = getattr(error, "reason", error)
            raise CatalogSyncError(f"provider catalog request failed: {reason}") from None
        with response:
            status = response.getcode()
            if status != 200:
                raise CatalogSyncError(
                    f"provider catalog request returned HTTP {status}"
                )
            return _read_bounded(response, maximum, "provider catalog response")
    raise CatalogSyncError("provider catalog redirect limit exceeded")


def _immutable_asset_url(release_id: str, asset_name: str) -> str:
    if RELEASE_ID_PATTERN.fullmatch(release_id) is None:
        raise CatalogSyncError("release id has an invalid format")
    return f"{RELEASE_DOWNLOAD_ROOT}{release_id}/{asset_name}"


def resolve_release(
    release_id: Optional[str] = None,
    fetch=fetch_github_asset,  # noqa: ANN001
) -> CatalogRelease:
    latest_bytes: Optional[bytes] = None
    if release_id is None:
        latest_bytes = fetch(LATEST_MANIFEST_URL, MANIFEST_LIMIT_BYTES)
        release_id = parse_manifest(latest_bytes).release_id
    elif RELEASE_ID_PATTERN.fullmatch(release_id) is None:
        raise CatalogSyncError("release id has an invalid format")

    immutable_manifest_bytes = fetch(
        _immutable_asset_url(release_id, MANIFEST_NAME), MANIFEST_LIMIT_BYTES
    )
    manifest = parse_manifest(immutable_manifest_bytes)
    if manifest.release_id != release_id:
        raise CatalogSyncError("immutable manifest does not match its release tag")
    if latest_bytes is not None and immutable_manifest_bytes != latest_bytes:
        raise CatalogSyncError("latest manifest changed during immutable resolution")

    catalog_bytes = fetch(
        _immutable_asset_url(release_id, CATALOG_NAME), ARTIFACT_LIMIT_BYTES
    )
    validate_catalog(manifest, catalog_bytes)
    return CatalogRelease(
        manifest=manifest,
        manifest_bytes=immutable_manifest_bytes,
        catalog_bytes=catalog_bytes,
    )


def render_readme(manifest: ReleaseManifest) -> bytes:
    release_id = manifest.release_id
    text = f"""# Embedded provider catalog

These files are copied byte-for-byte from the immutable GitHub Release
[`{release_id}`](https://github.com/{CATALOG_REPOSITORY}/releases/tag/{release_id})
in the public
[`{CATALOG_REPOSITORY}`](https://github.com/{CATALOG_REPOSITORY})
repository.

- `catalog-v1.json`: `{manifest.catalog.byte_len}` bytes,
  SHA-256 `{manifest.catalog.sha256}`
- `manifest-v1.json`: the release manifest used to authenticate the catalog
  identity and digest

The runtime embeds both files so a fresh install has a complete offline
baseline. Interactive sessions perform a bounded, best-effort GitHub refresh
when due; headless commands stay offline unless refresh is explicitly requested.

Do not hand-edit these generated files. Maintainers update to GitHub's latest
stable catalog with:

```console
python3 scripts/sync_provider_catalog.py
python3 scripts/sync_provider_catalog.py --check
```

`--release-id <id>` selects an immutable release explicitly. New tag builds
must pass the latest-release check before compiling. A manual rebuild is only
allowed for an existing release and validates that tag's committed manifest
identity, catalog digest, and generated README entirely offline.
Source observations and field-level provenance remain in the catalog repository
and its release assets; Euler packages no provider credentials or observation
responses.
"""
    return text.encode("utf-8")


def desired_files(repo_root: Path, release: CatalogRelease) -> dict[Path, bytes]:
    catalog_dir = repo_root / "crates" / "euler-provider" / "catalog"
    return {
        catalog_dir / CATALOG_NAME: release.catalog_bytes,
        catalog_dir / MANIFEST_NAME: release.manifest_bytes,
        catalog_dir / "README.md": render_readme(release.manifest),
    }


def check_embedded(repo_root: Path, release: CatalogRelease) -> None:
    mismatches = []
    for path, expected in desired_files(repo_root, release).items():
        try:
            actual = path.read_bytes()
        except OSError:
            mismatches.append(str(path.relative_to(repo_root)))
            continue
        if actual != expected:
            mismatches.append(str(path.relative_to(repo_root)))
    if mismatches:
        joined = ", ".join(mismatches)
        raise CatalogSyncError(
            f"embedded provider catalog is not {release.manifest.release_id}: {joined}; "
            "run scripts/sync_provider_catalog.py before tagging"
        )


def _stage_file(path: Path, data: bytes, sequence: int) -> Path:
    temporary = path.with_name(f".{path.name}.{os.getpid()}.{sequence}.tmp")
    mode = path.stat().st_mode & 0o777 if path.exists() else 0o644
    descriptor = os.open(temporary, os.O_WRONLY | os.O_CREAT | os.O_EXCL, mode)
    try:
        with os.fdopen(descriptor, "wb") as handle:
            handle.write(data)
            handle.flush()
            os.fsync(handle.fileno())
    except BaseException:
        try:
            temporary.unlink()
        except OSError:
            pass
        raise
    return temporary


def install_release(repo_root: Path, release: CatalogRelease) -> None:
    files = desired_files(repo_root, release)
    for path in files:
        if not path.parent.is_dir():
            raise CatalogSyncError(f"expected catalog directory is missing: {path.parent}")

    staged: list[tuple[Path, Path]] = []
    try:
        for sequence, (target, data) in enumerate(files.items()):
            staged.append((target, _stage_file(target, data, sequence)))
        # Tracked files are individually atomically replaced, with the manifest
        # last. An interrupted multi-file update remains visible to git and is
        # repaired by rerunning this command; it is not a filesystem transaction.
        staged.sort(key=lambda item: item[0].name == MANIFEST_NAME)
        for target, temporary in staged:
            os.replace(temporary, target)
        if os.name != "nt":
            directory_fd = os.open(next(iter(files)).parent, os.O_RDONLY)
            try:
                os.fsync(directory_fd)
            finally:
                os.close(directory_fd)
    except OSError as error:
        raise CatalogSyncError(f"could not update embedded catalog: {error}") from error
    finally:
        for _, temporary in staged:
            try:
                temporary.unlink()
            except FileNotFoundError:
                pass


def workspace_version(repo_root: Path) -> tuple[int, int, int]:
    try:
        lines = (repo_root / "Cargo.toml").read_text(encoding="utf-8").splitlines()
    except (OSError, UnicodeError) as error:
        raise CatalogSyncError("could not read workspace version from Cargo.toml") from error
    section = ""
    versions = []
    for line in lines:
        stripped = line.strip()
        if stripped.startswith("[") and stripped.endswith("]"):
            section = stripped
            continue
        if section != "[workspace.package]":
            continue
        match = re.fullmatch(r'version\s*=\s*"([^"]+)"\s*(?:#.*)?', stripped)
        if match:
            versions.append(match.group(1))
    if len(versions) != 1:
        raise CatalogSyncError("could not read workspace version from Cargo.toml")
    return _parse_semver(
        versions[0],
        "workspace.package.version",
    )


def load_embedded_release(repo_root: Path) -> CatalogRelease:
    catalog_dir = repo_root / "crates" / "euler-provider" / "catalog"
    try:
        manifest_bytes = (catalog_dir / MANIFEST_NAME).read_bytes()
        catalog_bytes = (catalog_dir / CATALOG_NAME).read_bytes()
    except OSError as error:
        raise CatalogSyncError(f"could not read embedded catalog: {error}") from error
    manifest = parse_manifest(manifest_bytes)
    validate_catalog(manifest, catalog_bytes)
    return CatalogRelease(manifest, manifest_bytes, catalog_bytes)


def parse_args(argv: list[str]) -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    mode = parser.add_mutually_exclusive_group()
    mode.add_argument(
        "--check",
        action="store_true",
        help="verify tracked files match latest (or --release-id); write nothing",
    )
    mode.add_argument(
        "--check-pinned",
        action="store_true",
        help="validate the tracked catalog against its committed manifest and digest",
    )
    parser.add_argument(
        "--release-id",
        help="use this immutable catalog release instead of GitHub's latest release",
    )
    args = parser.parse_args(argv)
    if args.check_pinned and args.release_id:
        parser.error("--check-pinned cannot be combined with --release-id")
    return args


def run(argv: list[str]) -> int:
    args = parse_args(argv)
    repo_root = Path(__file__).resolve().parents[1]
    release = (
        load_embedded_release(repo_root)
        if args.check_pinned
        else resolve_release(args.release_id)
    )
    minimum = _parse_semver(
        release.manifest.minimum_euler_version, "manifest.minimum_euler_version"
    )
    current = workspace_version(repo_root)
    if current < minimum:
        raise CatalogSyncError(
            f"catalog requires Euler {release.manifest.minimum_euler_version}, "
            f"but this workspace is {'.'.join(str(part) for part in current)}"
        )

    if args.check or args.check_pinned:
        check_embedded(repo_root, release)
        print(f"embedded provider catalog is current: {release.manifest.release_id}")
    else:
        install_release(repo_root, release)
        print(f"updated embedded provider catalog: {release.manifest.release_id}")
    return 0


def main() -> int:
    try:
        return run(sys.argv[1:])
    except CatalogSyncError as error:
        print(f"provider catalog sync failed: {error}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
