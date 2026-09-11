#!/usr/bin/env python3
"""Embed each crate's CycloneDX SBOM inside its wheel.

A wheel that ships without its SBOM makes a consumer guess what went into the
artifact. The parser repository's own release note is that `json_fast-0.1.0`
once meant two different things in two repositories with no way to tell them
apart, so the bill of materials travels with the bytes it describes.

Placement follows the convention the previously published wheels used:
``<distribution>.dist-info/sboms/<crate>.cyclonedx.json``.

Wheels are rebuilt in place, so run this before generating the release manifest
— injecting changes the archive and therefore its SHA-256.

Usage:
    python3 scripts/inject_sbom.py [--dist dist]
"""

from __future__ import annotations

import argparse
import json
import shutil
import tempfile
import zipfile
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]

#: Wheel distribution name -> the crate whose SBOM belongs in it.
SBOM_SOURCES: dict[str, str] = {
    "json_fast": "json-fast-core",
    "xsd_fast": "xsd-fast-core",
    "docx_fast": "docx-fast-core",
    "email_fast": "email-fast-core",
    "xlsx_fast": "xlsx-fast-core",
    "pptx_fast": "pptx-fast-core",
}


def crate_sbom(crate: str) -> Path:
    """Generate the crate's SBOM if it is not already present."""

    candidate = ROOT / crate / f"{crate}.cdx.json"
    return candidate


def distribution_name(archive: zipfile.ZipFile) -> str:
    metadata = next(
        name for name in archive.namelist() if name.endswith(".dist-info/METADATA")
    )
    for line in archive.read(metadata).decode("utf-8").splitlines():
        if line.startswith("Name: "):
            return line[6:].strip()
    raise SystemExit("wheel carries no Name in METADATA")


def inject(wheel: Path, sbom: Path) -> bool:
    """Rewrite the wheel with the SBOM added. Returns False if already present."""

    with zipfile.ZipFile(wheel) as archive:
        distribution = distribution_name(archive)
        crate = SBOM_SOURCES.get(distribution.replace("-", "_"))
        if crate is None:
            raise SystemExit(f"{wheel.name}: no crate maps to {distribution!r}")

        # dist-info keeps the dashed distribution name as published.
        info = next(
            name.split("/")[0] for name in archive.namelist() if name.endswith(".dist-info/METADATA")
        )
        member = f"{info}/sboms/{crate}.cyclonedx.json"
        if member in archive.namelist():
            return False

        payload = sbom.read_bytes()
        # A malformed SBOM must not be shipped as if it were valid.
        json.loads(payload)

        entries = {
            name: archive.read(name)
            for name in archive.namelist()
            # Drop any SBOM that reached the wheel another way — a stray
            # `cargo cyclonedx` in the binding crate before `maturin build` is
            # enough for one to be packaged. Injection owns this set, so what a
            # wheel carries is decided in one place.
            if "/sboms/" not in name
        }

    entries[member] = payload

    with tempfile.NamedTemporaryFile(dir=wheel.parent, suffix=".whl", delete=False) as handle:
        temporary = Path(handle.name)

    with zipfile.ZipFile(temporary, "w", zipfile.ZIP_DEFLATED) as out:
        for name, blob in sorted(entries.items()):
            # Deterministic timestamps, matching the bundle packer's convention.
            info = zipfile.ZipInfo(name, date_time=(1980, 1, 1, 0, 0, 0))
            info.compress_type = zipfile.ZIP_DEFLATED
            out.writestr(info, blob)

    shutil.move(str(temporary), wheel)
    return True


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--dist", default="dist")
    arguments = parser.parse_args()

    dist = (ROOT / arguments.dist).resolve()
    wheels = sorted(dist.glob("*.whl"))
    if not wheels:
        raise SystemExit(f"no wheels in {dist}; build them first")

    failures: list[str] = []
    for wheel in wheels:
        with zipfile.ZipFile(wheel) as archive:
            distribution = distribution_name(archive)
        crate = SBOM_SOURCES.get(distribution.replace("-", "_"))
        if crate is None:
            failures.append(f"{wheel.name}: unmapped distribution {distribution!r}")
            continue
        sbom = crate_sbom(crate)
        if not sbom.is_file():
            failures.append(
                f"{wheel.name}: no SBOM at {sbom.relative_to(ROOT)}; run "
                f"`cargo cyclonedx --manifest-path {crate}/Cargo.toml -f json`"
            )
            continue
        changed = inject(wheel, sbom)
        print(f"  {'injected' if changed else 'already present'}: {wheel.name} <- {sbom.name}")

    if failures:
        for failure in failures:
            print(f"  {failure}")
        raise SystemExit("SBOM injection incomplete")

    # The archive changed, so any hash recorded earlier is now stale.
    manifest = dist / "release-manifest.json"
    if manifest.is_file():
        manifest.unlink()
        print("  removed release-manifest.json; regenerate it after this step")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
