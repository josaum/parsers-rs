"""Every Python binding's three names agree with the release tooling.

A binding has a crate (`mcts-fast-py`), a distribution (`[project] name`,
here `mcts-fast`, which names its wheel) and an import name (`[tool.maturin]
module-name`, here `mcts_fast_py`). The release maps wheels to crates by
distribution, finds each wheel's SBOM source the same way, and imports each
installed wheel by import name. A mismatch passes CI (whose binding jobs
build only the packages that have tests) and fails the tag.
"""

from __future__ import annotations

import re
import sys
import tomllib
from pathlib import Path

import pytest

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT / "scripts"))

from binding_modules import import_name  # noqa: E402
from inject_sbom import SBOM_SOURCES  # noqa: E402
from release_manifest import CRATES  # noqa: E402

RELEASE = ROOT / ".github/workflows/release.yml"


def released_bindings() -> list[str]:
    found = re.search(r"^  BINDINGS: (.+)$", RELEASE.read_text(encoding="utf-8"), re.MULTILINE)
    assert found, "release.yml declares no BINDINGS"
    return found[1].split()


def distribution(crate: str) -> str:
    project = tomllib.loads((ROOT / crate / "pyproject.toml").read_text(encoding="utf-8"))
    return re.sub(r"[-_.]+", "_", project["project"]["name"]).lower()


def core_crate(crate: str) -> str:
    return crate.removesuffix("-py") if crate.endswith("-write-py") else crate.replace("-py", "-core")


def test_every_python_binding_is_released():
    with_pyproject = sorted(path.parent.name for path in ROOT.glob("*-py/pyproject.toml"))
    assert sorted(released_bindings()) == with_pyproject


@pytest.mark.parametrize("crate", released_bindings())
def test_the_manifest_maps_the_wheel_distribution_to_the_crate(crate: str):
    assert CRATES.get(distribution(crate)) == crate


@pytest.mark.parametrize("crate", released_bindings())
def test_the_sbom_source_is_found_by_distribution(crate: str):
    assert SBOM_SOURCES.get(distribution(crate)) == core_crate(crate)
    assert (ROOT / core_crate(crate) / "Cargo.toml").is_file()


@pytest.mark.parametrize("crate", released_bindings())
def test_the_import_name_is_a_package_the_wheel_ships(crate: str):
    name = import_name(crate)
    assert (ROOT / crate / name / "__init__.py").is_file(), f"{crate} ships no {name}/__init__.py"


def test_the_tables_hold_nothing_unreleased():
    distributions = {distribution(crate) for crate in released_bindings()}
    assert set(CRATES) == distributions
    assert set(SBOM_SOURCES) == distributions
