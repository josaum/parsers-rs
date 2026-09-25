#!/usr/bin/env python3
"""Publish a versioned, provenance-stamped release of every parser wheel.

The failure this exists to prevent: two repositories built wheels named
``json_fast-0.1.0`` from sources that differed by 242 lines, and the artifact
carried no pointer to which source produced it. A consumer could not tell, and
neither could a reviewer.

So a release here is not "some wheels in a directory". It is a **manifest** that
binds, for every artifact:

* the wheel file name, its SHA-256, and its size,
* the crate and Python version it reports,
* the exact source commit and whether the tree was dirty at build time,
* the Rust toolchain and target triple, and
* a CycloneDX SBOM when ``cargo-cyclonedx`` is installed.

The manifest is refused, not written, when the inputs disagree: a wheel whose
reported version does not match its crate, or a dirty tree without
``--allow-dirty``, is an artifact nobody can reproduce.

Usage:
    python3 scripts/release_manifest.py --out dist [--allow-dirty]
"""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import re
import shutil
import subprocess
import sys
import tarfile
import zipfile
from datetime import UTC, datetime
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
MANIFEST_NAME = "release-manifest.json"
SCHEMA_VERSION = "parsers-rs.release.v1"

# Wheel file name -> the crate directory whose version it must report.
NODE_CRATES: set[str] = {"docx-fast-node", "xlsx-fast-node", "pptx-fast-node", "email-fast-node"}

CRATES: dict[str, str] = {
    "json_fast": "json-fast-py",
    "xsd_fast": "xsd-fast-py",
    "docx_fast": "docx-fast-py",
    "email_fast": "email-fast-py",
    "xlsx_fast": "xlsx-fast-py",
    "pptx_fast": "pptx-fast-py",
    "pdf_fast": "pdf-fast-py",
}


def run(argv: list[str], *, cwd: Path = ROOT) -> str:
    completed = subprocess.run(
        argv, cwd=cwd, text=True, capture_output=True, check=False
    )
    if completed.returncode != 0:
        raise SystemExit(f"command failed: {' '.join(argv)}\n{completed.stderr}")
    return completed.stdout.strip()


def source_commit() -> str:
    try:
        return run(["git", "rev-parse", "HEAD"])
    except SystemExit:
        return "unknown"


def source_is_dirty() -> bool:
    try:
        return bool(run(["git", "status", "--porcelain"]))
    except SystemExit:
        return True


def crate_version(crate: str) -> str:
    manifest = (ROOT / crate / "Cargo.toml").read_text(encoding="utf-8")
    match = re.search(r'^version = "([^"]+)"', manifest, flags=re.M)
    if match is None:
        raise SystemExit(f"{crate}/Cargo.toml declares no version")
    return match.group(1)


def wheel_distribution(path: Path) -> tuple[str, str, str | None]:
    """Return (distribution, version, author) as the wheel itself reports them."""

    with zipfile.ZipFile(path) as archive:
        metadata_name = next(
            name for name in archive.namelist() if name.endswith(".dist-info/METADATA")
        )
        metadata = archive.read(metadata_name).decode("utf-8")
    name = re.search(r"^Name: (.+)$", metadata, flags=re.M)
    version = re.search(r"^Version: (.+)$", metadata, flags=re.M)
    author = re.search(r"^Author(?:-email)?: (.+)$", metadata, flags=re.M)
    if name is None or version is None:
        raise SystemExit(f"{path.name} carries no Name/Version metadata")
    return name.group(1).strip(), version.group(1).strip(), (author.group(1).strip() if author else None)


def wheel_sbom(path: Path) -> dict[str, str] | None:
    """Return the embedded SBOM's identity, or None when the wheel carries none."""

    with zipfile.ZipFile(path) as archive:
        members = [n for n in archive.namelist() if "/sboms/" in n and n.endswith(".json")]
        if not members:
            return None
        document = json.loads(archive.read(members[0]))
    component = (document.get("metadata") or {}).get("component") or {}
    return {
        "member": members[0],
        "spec_version": str(document.get("specVersion") or ""),
        "component": str(component.get("name") or ""),
        "component_version": str(component.get("version") or ""),
        "format": str(document.get("bomFormat") or ""),
    }



def tarball_distribution(path: Path) -> tuple[str, str, str | None]:
    with tarfile.open(path, "r:gz") as archive:
        try:
            member = archive.getmember("package/package.json")
            f = archive.extractfile(member)
            if f is None:
                raise KeyError
            metadata = json.loads(f.read().decode("utf-8"))
        except KeyError:
            raise SystemExit(f"{path.name} carries no package/package.json")
        name = metadata.get("name")
        version = metadata.get("version")
        author = metadata.get("author")
        if not name or not version:
            raise SystemExit(f"{path.name} package.json is missing name/version")
        return name, version, author


def digest(path: Path) -> str:
    hasher = hashlib.sha256()
    with path.open("rb") as handle:
        for chunk in iter(lambda: handle.read(1024 * 1024), b""):
            hasher.update(chunk)
    return hasher.hexdigest()


def cyclonedx_available() -> bool:
    return shutil.which("cargo-cyclonedx") is not None


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--out", default="dist", help="directory holding built wheels")
    parser.add_argument(
        "--allow-dirty",
        action="store_true",
        help="record a dirty tree instead of refusing to publish it",
    )
    arguments = parser.parse_args()

    out = (ROOT / arguments.out).resolve()
    wheels = sorted(out.glob("*.whl"))
    tarballs = sorted(out.glob("*.tgz"))
    if not wheels and not tarballs:
        raise SystemExit(f"no artifacts in {out}; build them first")

    dirty = source_is_dirty()
    if dirty and not arguments.allow_dirty:
        raise SystemExit(
            "the working tree has uncommitted changes, so these artifacts cannot "
            "be reproduced from a commit. Commit first, or pass --allow-dirty to "
            "record the dirty state explicitly."
        )

    artifacts = []
    for artifact_path in wheels + tarballs:
        is_wheel = artifact_path.name.endswith(".whl")
        if is_wheel:
            distribution, version, author = wheel_distribution(artifact_path)
            key = distribution.replace("-", "_")
            crate = CRATES.get(key)
        else:
            distribution, version, author = tarball_distribution(artifact_path)
            crate = distribution
            if crate not in NODE_CRATES:
                crate = None

        if crate is None:
            raise SystemExit(f"{artifact_path.name}: no crate maps to distribution {distribution!r}")
        
        declared = crate_version(crate)
        if version != declared:
            raise SystemExit(
                f"{artifact_path.name} reports version {version} but {crate} declares "
                f"{declared}; a consumer pinning {version} would get other bytes"
            )
        if author and author.strip() != "Dionisio Chiuratto Agourakis":
            raise SystemExit(
                f"{artifact_path.name} is attributed to {author!r}. A shared artifact must "
                "name this repository, so a consumer can trace it back."
            )
        artifacts.append(
            {
                "sbom": wheel_sbom(artifact_path) if is_wheel else None,
                "distribution": distribution,
                "crate": crate,
                "version": version,
                "author": author,
                "file": artifact_path.name,
                "sha256": digest(artifact_path),
                "bytes": artifact_path.stat().st_size,
            }
        )

    manifest = {
        "schema_version": SCHEMA_VERSION,
        "generated_at": datetime.now(UTC).isoformat().replace("+00:00", "Z"),
        "source": {
            "commit": source_commit(),
            "dirty": dirty,
        },
        "build": {
            "rustc": run(["rustc", "--version"]),
            "cargo": run(["cargo", "--version"]),
            "target": run(["rustc", "-vV"]).split("host: ")[-1].splitlines()[0],
            "python_abi": "abi3-py38",
        },
        "sbom": {
            "tool": "cargo-cyclonedx" if cyclonedx_available() else None,
            "embedded": all(artifact["sbom"] for artifact in artifacts if artifact["file"].endswith(".whl")),
            "note": (
                None
                if all(artifact["sbom"] for artifact in artifacts if artifact["file"].endswith(".whl"))
                else "One or more wheels carry no embedded SBOM. Run "
                "scripts/inject_sbom.py before generating this manifest."
            ),
        },
        "artifacts": artifacts,
    }

    destination = out / MANIFEST_NAME
    destination.write_text(json.dumps(manifest, indent=2, sort_keys=False) + "\n", encoding="utf-8")
    print(f"wrote {destination}")
    for artifact in artifacts:
        print(f"  {artifact['file']}  sha256={artifact['sha256'][:16]}…  {artifact['bytes']} bytes")
    if not cyclonedx_available():
        print("  note: no SBOM written (cargo-cyclonedx missing)", file=sys.stderr)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
