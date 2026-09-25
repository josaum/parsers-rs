"""The release manifest must describe the artifacts it ships, or be absent.

Provenance that is only asserted in prose is not provenance. This suite holds
`dist/release-manifest.json` to the wheels actually present, so a manifest that
outlives the artifacts it names fails here instead of misleading a consumer.

It runs only when a `dist/` exists; a checkout that has not built wheels has no
release to verify. Build one with:

    (cd json-fast-py && maturin build --release --out ../dist)
    (cd xsd-fast-py  && maturin build --release --out ../dist)
    python3 scripts/release_manifest.py --out dist
"""

from __future__ import annotations

import hashlib
import json
import re
import subprocess
import sys
import zipfile
from pathlib import Path

import pytest

ROOT = Path(__file__).resolve().parents[1]
DIST = ROOT / "dist"
MANIFEST = DIST / "release-manifest.json"

pytestmark = pytest.mark.skipif(
    not MANIFEST.is_file(),
    reason="no release manifest; this checkout has not built a release",
)

sys.path.insert(0, str(ROOT / "scripts"))
from release_manifest import CRATES, crate_version, wheel_distribution  # noqa: E402


@pytest.fixture(scope="module")
def manifest() -> dict:
    return json.loads(MANIFEST.read_text(encoding="utf-8"))


def sha256(path: Path) -> str:
    hasher = hashlib.sha256()
    with path.open("rb") as handle:
        for chunk in iter(lambda: handle.read(1 << 20), b""):
            hasher.update(chunk)
    return hasher.hexdigest()


def test_every_recorded_artifact_exists_and_hashes_as_recorded(manifest: dict) -> None:
    for artifact in manifest["artifacts"]:
        wheel = DIST / artifact["file"]
        assert wheel.is_file(), f"{artifact['file']} is recorded but absent"
        assert sha256(wheel) == artifact["sha256"], (
            f"{artifact['file']} does not match the manifest hash; the artifact "
            "changed after the release was stamped"
        )
        assert wheel.stat().st_size == artifact["bytes"]


def test_no_artifact_is_missing_from_the_manifest(manifest: dict) -> None:
    recorded = {artifact["file"] for artifact in manifest["artifacts"]}
    present = {wheel.name for wheel in DIST.glob("*.whl")}
    assert present - recorded == set(), (
        f"unrecorded wheels present: {sorted(present - recorded)}; they have no "
        "provenance and must not be published alongside a stamped release"
    )


def test_manifest_versions_match_the_crate_manifests(manifest: dict) -> None:
    for artifact in manifest["artifacts"]:
        crate = artifact["crate"]
        assert crate in CRATES.values(), f"{crate} is not a known crate"
        assert artifact["version"] == crate_version(crate), (
            f"{artifact['file']} claims {artifact['version']} but {crate} "
            "declares a different version"
        )


def test_manifest_pins_the_source_commit(manifest: dict) -> None:
    commit = manifest["source"]["commit"]
    assert re.fullmatch(r"[0-9a-f]{40}", commit), (
        f"manifest carries no usable source commit: {commit!r}; an artifact "
        "nobody can trace to a revision is the failure this repo exists to fix"
    )
    head = subprocess.run(
        ["git", "rev-parse", "HEAD"], cwd=ROOT, capture_output=True, text=True, check=False
    ).stdout.strip()
    if head:
        # A manifest may legitimately predate the current commit, but it must
        # never name a commit this repository does not contain.
        known = subprocess.run(
            ["git", "cat-file", "-e", f"{commit}^{{commit}}"],
            cwd=ROOT,
            capture_output=True,
            text=True,
            check=False,
        )
        assert known.returncode == 0, f"manifest names unknown commit {commit}"


def test_artifacts_are_attributed_to_this_repository(manifest: dict) -> None:
    for artifact in manifest["artifacts"]:
        author = artifact.get("author") or ""
        assert author == "Dionisio Chiuratto Agourakis", (
            f"{artifact['file']} is attributed to someone else ({author!r}); a "
            "shared artifact must name the project that produced it"
        )


def test_wheel_metadata_agrees_with_the_manifest(manifest: dict) -> None:
    for artifact in manifest["artifacts"]:
        wheel = DIST / artifact["file"]
        with zipfile.ZipFile(wheel) as archive:
            metadata_name = next(
                name for name in archive.namelist() if name.endswith(".dist-info/METADATA")
            )
            metadata = archive.read(metadata_name).decode("utf-8")
        distribution, version, _author = wheel_distribution(wheel)
        assert distribution == artifact["distribution"]
        assert version == artifact["version"]
        assert f"Version: {version}" in metadata
