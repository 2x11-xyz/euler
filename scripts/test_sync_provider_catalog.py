from __future__ import annotations

import dataclasses
import hashlib
import io
import json
from pathlib import Path
import tempfile
from typing import Optional
import unittest
from unittest import mock

from scripts import sync_provider_catalog as sync


REPO_ROOT = Path(__file__).resolve().parents[1]


PUBLISHED_MANIFEST = b"""{
  "artifacts": {
    "catalog-v1.json": {
      "bytes": 152593,
      "sha256": "bf3cb671579a1008d81cc15180a37311c8c864a9d073f16009bcbddfdb1ff197"
    },
    "provenance-v1.json": {
      "bytes": 7206,
      "sha256": "bcdcd39626d0de12d1669626c1f71d711f2833fac100ba78027a08ac2c8d206f"
    }
  },
  "generated_at": "2026-07-21T00:01:30Z",
  "minimum_euler_version": "0.1.1",
  "release_id": "catalog-v1-20260721t000130z-2834472ff45442f2617675c8e2ed9c2cbca2e33b2e193c799cb4e68e49a2683b",
  "schema_version": 1
}
"""


def make_release(
    catalog: bytes = b'{"providers":{"fixture":{}},"schema_version":1}\n',
) -> sync.CatalogRelease:
    base = sync.ReleaseManifest(
        schema_version=1,
        release_id="pending",
        generated_at="2026-07-21T01:02:03Z",
        minimum_euler_version="0.1.1",
        catalog=sync.ArtifactMetadata(
            byte_len=len(catalog), sha256=hashlib.sha256(catalog).hexdigest()
        ),
        provenance=sync.ArtifactMetadata(
            byte_len=1, sha256=hashlib.sha256(b"p").hexdigest()
        ),
    )
    manifest = dataclasses.replace(base, release_id=sync.expected_release_id(base))
    value = manifest.identity_value()
    value["release_id"] = manifest.release_id
    manifest_bytes = f"{json.dumps(value, indent=2, sort_keys=True)}\n".encode()
    return sync.CatalogRelease(manifest, manifest_bytes, catalog)


class FakeResponse:
    def __init__(self, data: bytes, content_length: Optional[str] = None):
        self._stream = io.BytesIO(data)
        self.headers = {}
        if content_length is not None:
            self.headers["Content-Length"] = content_length

    def read(self, amount: int) -> bytes:
        return self._stream.read(amount)


class ManifestTests(unittest.TestCase):
    def test_published_release_is_a_cross_repo_identity_vector(self) -> None:
        manifest = sync.parse_manifest(PUBLISHED_MANIFEST)

        self.assertEqual(
            sync.expected_release_id(manifest),
            "catalog-v1-20260721t000130z-2834472ff45442f2617675c8e2ed9c2cbca2e33b2e193c799cb4e68e49a2683b",
        )
        self.assertEqual(
            hashlib.sha256(sync.canonical_manifest_identity(manifest)).hexdigest(),
            "2834472ff45442f2617675c8e2ed9c2cbca2e33b2e193c799cb4e68e49a2683b",
        )

    def test_duplicate_nested_key_is_rejected(self) -> None:
        with self.assertRaisesRegex(sync.CatalogSyncError, "duplicate JSON key 'x'"):
            sync._strict_json_object(b'{"outer":{"x":1,"x":2}}', "fixture")

    def test_non_json_numeric_constants_are_rejected(self) -> None:
        with self.assertRaisesRegex(sync.CatalogSyncError, "non-JSON numeric"):
            sync._strict_json_object(b'{"value":NaN}', "fixture")

    def test_unknown_manifest_field_is_rejected(self) -> None:
        value = json.loads(PUBLISHED_MANIFEST)
        value["surprise"] = True

        with self.assertRaisesRegex(sync.CatalogSyncError, "unknown"):
            sync.parse_manifest(json.dumps(value).encode())

    def test_one_byte_identity_change_is_rejected(self) -> None:
        value = json.loads(PUBLISHED_MANIFEST)
        value["minimum_euler_version"] = "0.1.2"

        with self.assertRaisesRegex(sync.CatalogSyncError, "does not authenticate"):
            sync.parse_manifest(json.dumps(value).encode())


class UrlAndBoundTests(unittest.TestCase):
    def test_runtime_github_release_hosts_are_allowed(self) -> None:
        for host in sorted(sync.ALLOWED_GITHUB_HOSTS):
            sync.validate_github_url(f"https://{host}/asset?signature=value")

    def test_urls_outside_exact_https_boundary_are_rejected(self) -> None:
        invalid = [
            "http://github.com/asset",
            "https://example.com/asset",
            "https://user@github.com/asset",
            "https://github.com:444/asset",
            "https://github.com/asset#fragment",
        ]
        for url in invalid:
            with self.subTest(url=url), self.assertRaises(sync.CatalogSyncError):
                sync.validate_github_url(url)

    def test_bounded_read_accepts_exact_limit_without_content_length(self) -> None:
        self.assertEqual(
            sync._read_bounded(FakeResponse(b"abcd"), 4, "fixture"), b"abcd"
        )

    def test_bounded_read_rejects_streamed_overflow_and_lies(self) -> None:
        with self.assertRaisesRegex(sync.CatalogSyncError, "exceeds"):
            sync._read_bounded(FakeResponse(b"abcde"), 4, "fixture")
        with self.assertRaisesRegex(sync.CatalogSyncError, "did not match"):
            sync._read_bounded(FakeResponse(b"abcd", "3"), 4, "fixture")
        with self.assertRaisesRegex(sync.CatalogSyncError, "Content-Length is invalid"):
            sync._read_bounded(FakeResponse(b"abcd", "+4"), 4, "fixture")

    def test_redirect_target_is_joined_then_revalidated(self) -> None:
        self.assertEqual(
            sync._redirect_target(
                "https://github.com/owner/repo/releases/latest", "../asset", 0
            ),
            "https://github.com/owner/repo/asset",
        )
        self.assertEqual(
            sync._redirect_target(
                "https://github.com/asset",
                "https://release-assets.githubusercontent.com/signed?token=value",
                1,
            ),
            "https://release-assets.githubusercontent.com/signed?token=value",
        )

    def test_redirect_target_fails_on_missing_untrusted_or_excessive_hops(self) -> None:
        cases = [
            (None, 0, "omitted Location"),
            ("https://example.com/asset", 0, "outside"),
            ("/still-github", sync.MAX_REDIRECTS, "limit exceeded"),
        ]
        for location, count, message in cases:
            with self.subTest(location=location), self.assertRaisesRegex(
                sync.CatalogSyncError, message
            ):
                sync._redirect_target("https://github.com/start", location, count)


class ResolutionTests(unittest.TestCase):
    def test_latest_is_only_a_pointer_to_byte_identical_immutable_assets(self) -> None:
        release = make_release()
        manifest_url = sync._immutable_asset_url(
            release.manifest.release_id, sync.MANIFEST_NAME
        )
        catalog_url = sync._immutable_asset_url(
            release.manifest.release_id, sync.CATALOG_NAME
        )
        responses = {
            sync.LATEST_MANIFEST_URL: release.manifest_bytes,
            manifest_url: release.manifest_bytes,
            catalog_url: release.catalog_bytes,
        }
        calls: list[str] = []

        def fetch(url: str, maximum: int) -> bytes:
            calls.append(url)
            self.assertLessEqual(len(responses[url]), maximum)
            return responses[url]

        resolved = sync.resolve_release(fetch=fetch)

        self.assertEqual(resolved, release)
        self.assertEqual(calls, [sync.LATEST_MANIFEST_URL, manifest_url, catalog_url])

    def test_latest_immutable_manifest_disagreement_fails_closed(self) -> None:
        release = make_release()
        responses = {
            sync.LATEST_MANIFEST_URL: release.manifest_bytes,
            sync._immutable_asset_url(
                release.manifest.release_id, sync.MANIFEST_NAME
            ): release.manifest_bytes + b"\n",
        }

        def fetch(url: str, maximum: int) -> bytes:
            return responses[url]

        with self.assertRaisesRegex(sync.CatalogSyncError, "changed"):
            sync.resolve_release(fetch=fetch)

    def test_bad_catalog_bytes_fail_before_install(self) -> None:
        release = make_release()
        responses = {
            sync._immutable_asset_url(
                release.manifest.release_id, sync.MANIFEST_NAME
            ): release.manifest_bytes,
            sync._immutable_asset_url(
                release.manifest.release_id, sync.CATALOG_NAME
            ): release.catalog_bytes + b"x",
        }

        def fetch(url: str, maximum: int) -> bytes:
            return responses[url]

        with self.assertRaisesRegex(sync.CatalogSyncError, "byte length"):
            sync.resolve_release(release.manifest.release_id, fetch=fetch)


class InstallationTests(unittest.TestCase):
    def test_install_check_and_second_install_are_byte_identical(self) -> None:
        release = make_release()
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            catalog_dir = root / "crates" / "euler-provider" / "catalog"
            catalog_dir.mkdir(parents=True)
            for name in (sync.CATALOG_NAME, sync.MANIFEST_NAME, "README.md"):
                (catalog_dir / name).write_bytes(b"old")

            sync.install_release(root, release)
            first = {
                path: path.read_bytes() for path in sync.desired_files(root, release)
            }
            sync.check_embedded(root, release)
            sync.install_release(root, release)
            second = {
                path: path.read_bytes() for path in sync.desired_files(root, release)
            }

            self.assertEqual(first, second)
            self.assertFalse(list(catalog_dir.glob(".*.tmp")))
            self.assertEqual(sync.load_embedded_release(root), release)

    def test_check_names_every_stale_generated_file(self) -> None:
        release = make_release()
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            catalog_dir = root / "crates" / "euler-provider" / "catalog"
            catalog_dir.mkdir(parents=True)
            for name in (sync.CATALOG_NAME, sync.MANIFEST_NAME, "README.md"):
                (catalog_dir / name).write_bytes(b"old")

            with self.assertRaises(sync.CatalogSyncError) as raised:
                sync.check_embedded(root, release)

            message = str(raised.exception)
            self.assertIn(sync.CATALOG_NAME, message)
            self.assertIn(sync.MANIFEST_NAME, message)
            self.assertIn("README.md", message)


class WorkspaceVersionTests(unittest.TestCase):
    def test_workspace_version_reads_only_the_workspace_package_section(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            (root / "Cargo.toml").write_text(
                '[package]\nversion = "9.9.9"\n\n'
                '[workspace.package]\nversion = "0.1.2" # release version\n',
                encoding="utf-8",
            )

            self.assertEqual(sync.workspace_version(root), (0, 1, 2))

    def test_workspace_version_fails_closed_on_missing_or_duplicate_value(self) -> None:
        fixtures = (
            '[workspace.package]\nedition = "2024"\n',
            '[workspace.package]\nversion = "0.1.2"\nversion = "0.1.3"\n',
        )
        for contents in fixtures:
            with self.subTest(contents=contents), tempfile.TemporaryDirectory() as directory:
                root = Path(directory)
                (root / "Cargo.toml").write_text(contents, encoding="utf-8")
                with self.assertRaisesRegex(
                    sync.CatalogSyncError, "could not read workspace version"
                ):
                    sync.workspace_version(root)


class CommandModeTests(unittest.TestCase):
    def test_check_pinned_validates_only_the_committed_snapshot(self) -> None:
        output = io.StringIO()
        with mock.patch.object(
            sync,
            "resolve_release",
            side_effect=AssertionError("offline mode must not resolve GitHub releases"),
        ), mock.patch("sys.stdout", output):
            self.assertEqual(sync.run(["--check-pinned"]), 0)

        self.assertIn("embedded provider catalog is current", output.getvalue())


class WorkflowTests(unittest.TestCase):
    def test_release_builds_wait_for_catalog_validation(self) -> None:
        workflow = (REPO_ROOT / ".github" / "workflows" / "release.yml").read_text()

        self.assertIn("ref: refs/tags/${{ steps.tag.outputs.tag }}", workflow)
        self.assertIn("python3 scripts/sync_provider_catalog.py --check", workflow)
        self.assertIn("python3 scripts/sync_provider_catalog.py --check-pinned", workflow)
        self.assertIn("if: github.event_name == 'push'", workflow)
        self.assertIn("if: github.event_name == 'workflow_dispatch'", workflow)
        self.assertIn(
            'gh release view "$RELEASE_TAG" --repo "$GITHUB_REPOSITORY"', workflow
        )
        self.assertIn("RELEASE_TAG: ${{ steps.tag.outputs.tag }}", workflow)
        self.assertIn("needs: validate", workflow)
        self.assertIn(
            "ref: refs/tags/${{ needs.validate.outputs.tag }}", workflow
        )
        self.assertLess(
            workflow.index("Require the latest embedded provider catalog"),
            workflow.index("\n  build:"),
        )

    def test_routine_ci_runs_only_offline_sync_tests(self) -> None:
        workflow = (REPO_ROOT / ".github" / "workflows" / "ci.yml").read_text()

        self.assertIn(
            "python3 -m unittest scripts.test_sync_provider_catalog", workflow
        )
        self.assertNotIn("sync_provider_catalog.py --check", workflow)


if __name__ == "__main__":
    unittest.main()
