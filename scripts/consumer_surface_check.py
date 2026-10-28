#!/usr/bin/env python3
"""Measure whether a consumer's call sites exist on this release's Python wheels.

Before a consumer cuts over from a fork to a release, the question is not
"are the sources similar" but "does every attribute the consumer's code reads
exist on the installed module". This script answers that by installing the
release's wheels into a throwaway environment, importing each distribution's
top-level package, and diffing ``dir()`` (plus the native ``_core`` module)
against every ``<package>.<attr>`` the consumer's Python source names.

Usage::

    python3 scripts/consumer_surface_check.py \
        --wheels dist \
        --source ../consumer-workspace/cartridges \
        --source ../consumer-workspace/api \
        [--exclude office-parsers-rs] [--json]

Exit status is 1 when any referenced attribute is missing. The report lists,
per package, the attributes present, the attributes missing, and the files
that reference each missing one, so the gap is a work item and not a guess.

Requires ``uv`` (or falls back to ``python3 -m venv`` + ``pip``). Only wheels
matching the running platform are installed; the script says which ones.
"""

from __future__ import annotations

import argparse
import json
import os
import re
import shutil
import subprocess
import sys
import sysconfig
import tempfile
from collections import defaultdict
from pathlib import Path

# ``<package>.<attr>`` where <package> is a wheel's import name (``pdf_fast``).
# The trailing lookahead stops at a call, subscript, or end of token.
REF_RE = re.compile(r"\b([a-z][a-z0-9]*_fast)\.([A-Za-z_][A-Za-z0-9_]*)")
# ``from pdf_fast import a, b as c`` and the parenthesised multi-line form.
FROM_RE = re.compile(r"^\s*from\s+([a-z][a-z0-9]*_fast)\s+import\s+\(?([^)\n]*(?:\n[^)\n]*)*?)\)?\s*$", re.M)


def import_names(wheels: list[Path]) -> set[str]:
    return {w.name.split("-")[0] for w in wheels}


def platform_wheels(wheel_dir: Path) -> list[Path]:
    """Wheels this interpreter can install, deduplicated per distribution."""
    plat = sysconfig.get_platform().replace("-", "_").replace(".", "_")
    is_mac = plat.startswith("macosx")
    is_linux = plat.startswith("linux")
    is_win = plat.startswith("win")
    arch = plat.split("_")[-1]
    chosen: dict[str, Path] = {}
    for w in sorted(wheel_dir.glob("*.whl")):
        tag = w.stem.split("-")[-1]
        ok = (
            (is_mac and "macosx" in tag and arch in tag)
            or (is_linux and "manylinux" in tag and (arch in tag or (arch == "x86_64" and "x86_64" in tag)))
            or (is_win and plat in tag)
        )
        if ok:
            chosen.setdefault(w.name.split("-")[0], w)
    return list(chosen.values())


def venv_python(env: Path) -> Path:
    if sys.platform == "win32":
        return env / "Scripts" / "python.exe"
    return env / "bin" / "python"


def venv_pip(env: Path) -> Path:
    if sys.platform == "win32":
        return env / "Scripts" / "pip.exe"
    return env / "bin" / "pip"


def collect_references(sources: list[Path], exclude: list[str], packages: set[str]) -> dict[str, dict[str, set[str]]]:
    refs: dict[str, dict[str, set[str]]] = defaultdict(lambda: defaultdict(set))
    for root in sources:
        for path in root.rglob("*.py"):
            rel = str(path)
            rel_posix = f"/{path.as_posix().strip('/')}/"
            if any(
                skip in rel_posix
                for skip in ("/.venv/", "/node_modules/", "/target/", "/__pycache__/")
            ):
                continue
            if any(ex in rel or ex.replace("\\", "/") in rel_posix for ex in exclude):
                continue
            try:
                text = path.read_text(encoding="utf-8", errors="replace")
            except OSError:
                continue
            for pkg, attr in REF_RE.findall(text):
                if pkg in packages:
                    refs[pkg][attr].add(rel)
            for pkg, names in FROM_RE.findall(text):
                if pkg not in packages:
                    continue
                for item in names.replace("\\\n", " ").split(","):
                    name = item.strip().split(" as ")[0].strip()
                    if name and name != "*" and name.isidentifier():
                        refs[pkg][name].add(rel)
    return refs


def install_and_probe(wheels: list[Path], packages: set[str]) -> dict[str, dict[str, list[str]]]:
    """Install wheels in a temp env and return {package: {"top": [...], "core": [...]}}."""
    tmp = Path(tempfile.mkdtemp(prefix="parsers-surface-"))
    env = tmp / "venv"
    uv = shutil.which("uv")
    py_bin = venv_python(env)
    pip_bin = venv_pip(env)
    if uv:
        subprocess.run([uv, "venv", "-q", str(env)], check=True)
        subprocess.run([uv, "pip", "install", "-q", "--python", str(py_bin), *map(str, wheels)], check=True)
    else:
        subprocess.run([sys.executable, "-m", "venv", str(env)], check=True)
        subprocess.run([str(pip_bin), "install", "-q", *map(str, wheels)], check=True)
    probe = (
        "import importlib, json, sys\n"
        "out = {}\n"
        "for name in sys.argv[1:]:\n"
        "    m = importlib.import_module(name)\n"
        "    top = [a for a in dir(m) if not a.startswith('__')]\n"
        "    core = []\n"
        "    try:\n"
        "        c = importlib.import_module(name + '._core')\n"
        "        core = [a for a in dir(c) if not a.startswith('__')]\n"
        "    except ImportError:\n"
        "        pass\n"
        "    out[name] = {'top': top, 'core': core}\n"
        "print(json.dumps(out))\n"
    )
    result = subprocess.run(
        [str(py_bin), "-c", probe, *sorted(packages)],
        check=True,
        capture_output=True,
        text=True,
    )
    shutil.rmtree(tmp, ignore_errors=True)
    return json.loads(result.stdout)


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    parser.add_argument("--wheels", required=True, type=Path)
    parser.add_argument("--source", action="append", required=True, type=Path, help="consumer source root (repeatable)")
    parser.add_argument("--exclude", action="append", default=[], help="path fragment to skip, e.g. the consumer's own parser tree")
    parser.add_argument("--json", action="store_true", dest="as_json")
    args = parser.parse_args(argv)

    wheels = platform_wheels(args.wheels)
    if not wheels:
        print(f"no wheel in {args.wheels} matches this platform", file=sys.stderr)
        return 2
    packages = import_names(wheels)
    surface = install_and_probe(wheels, packages)
    refs = collect_references(args.source, args.exclude, packages)

    report: dict[str, dict] = {}
    missing_total = 0
    for pkg in sorted(packages):
        present = set(surface[pkg]["top"])
        # ``pkg._core.X`` references resolve against the native module.
        core = set(surface[pkg]["core"])
        used = refs.get(pkg, {})
        missing = {
            attr: sorted(files)
            for attr, files in sorted(used.items())
            if attr != "_core" and attr not in present
        }
        missing_total += len(missing)
        report[pkg] = {
            "wheel": next(w.name for w in wheels if w.name.startswith(pkg + "-")),
            "referenced": sorted(a for a in used if a != "_core"),
            "missing": missing,
            "core_exports": sorted(core),
        }

    if args.as_json:
        print(json.dumps(report, indent=2))
    else:
        for pkg, r in report.items():
            status = "ok" if not r["missing"] else f"{len(r['missing'])} missing"
            print(f"{pkg:12} {r['wheel']:60} referenced={len(r['referenced'])} {status}")
            for attr, files in r["missing"].items():
                print(f"    missing {pkg}.{attr}")
                for f in files[:4]:
                    print(f"        {f}")
                if len(files) > 4:
                    print(f"        … {len(files) - 4} more")
    return 1 if missing_total else 0


if __name__ == "__main__":
    sys.exit(main())
