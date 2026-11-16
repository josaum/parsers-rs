#!/usr/bin/env python3
"""Refuse a release artifact that ships without both license texts.

The project is licensed `MIT OR Apache-2.0`. Both licenses require their text
to travel with every copy, and consumers redistribute these wheels and tarballs
inside their own packages, so the notice has to be inside the artifact itself,
not only in the repository it was built from.

Every wheel must carry `<dist>.dist-info/licenses/LICENSE` and
`<dist>.dist-info/licenses/LICENSE-APACHE`; every Node tarball must carry
`package/LICENSE` and `package/LICENSE-APACHE`. Each must be byte-identical to
the copy at the repository root, so a stale or truncated text fails too.

Usage:
    python3 scripts/check_license_files.py [--dist dist]
"""

from __future__ import annotations

import argparse
import sys
import tarfile
import zipfile
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]

#: License file name at the repository root -> the same name inside artifacts.
LICENSE_FILES: tuple[str, ...] = ("LICENSE", "LICENSE-APACHE")


def expected_texts(root: Path = ROOT) -> dict[str, bytes]:
    return {name: (root / name).read_bytes() for name in LICENSE_FILES}


def wheel_problems(wheel: Path, expected: dict[str, bytes]) -> list[str]:
    problems: list[str] = []
    with zipfile.ZipFile(wheel) as archive:
        names = set(archive.namelist())
        dist_info = sorted(
            {name.split("/", 1)[0] for name in names if ".dist-info/" in name}
        )
        if len(dist_info) != 1:
            return [f"{wheel.name}: expected one .dist-info directory, found {dist_info}"]
        for name, text in expected.items():
            member = f"{dist_info[0]}/licenses/{name}"
            if member not in names:
                problems.append(f"{wheel.name}: missing {member}")
            elif archive.read(member) != text:
                problems.append(f"{wheel.name}: {member} differs from the root {name}")
    return problems


def tarball_problems(tarball: Path, expected: dict[str, bytes]) -> list[str]:
    problems: list[str] = []
    with tarfile.open(tarball, "r:gz") as archive:
        names = set(archive.getnames())
        for name, text in expected.items():
            member = f"package/{name}"
            if member not in names:
                problems.append(f"{tarball.name}: missing {member}")
                continue
            handle = archive.extractfile(member)
            if handle is None or handle.read() != text:
                problems.append(f"{tarball.name}: {member} differs from the root {name}")
    return problems


def check_dist(dist: Path, root: Path = ROOT) -> list[str]:
    expected = expected_texts(root)
    wheels = sorted(dist.glob("*.whl"))
    tarballs = sorted(dist.glob("*.tgz"))
    if not wheels and not tarballs:
        return [f"{dist}: no wheels or tarballs to check"]
    problems: list[str] = []
    for wheel in wheels:
        problems.extend(wheel_problems(wheel, expected))
    for tarball in tarballs:
        problems.extend(tarball_problems(tarball, expected))
    return problems


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--dist", type=Path, default=ROOT / "dist")
    args = parser.parse_args(argv)
    problems = check_dist(args.dist)
    for problem in problems:
        print(f"error: {problem}", file=sys.stderr)
    if problems:
        return 1
    print(f"license texts present in every artifact under {args.dist}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
