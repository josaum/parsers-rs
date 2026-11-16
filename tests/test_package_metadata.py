"""Every published package names this repository, so consumers can find the
source, the licence and the releases from the package alone."""

from __future__ import annotations

import json
import tomllib
from pathlib import Path

import pytest

ROOT = Path(__file__).resolve().parents[1]
REPOSITORY = "https://github.com/josaum/parsers-rs"
PYTHON_PACKAGES = sorted(ROOT.glob("*-py/pyproject.toml"))
NODE_PACKAGES = sorted(ROOT.glob("*-node/package.json"))


def test_there_are_packages_to_check() -> None:
    assert len(PYTHON_PACKAGES) >= 10 and len(NODE_PACKAGES) >= 10


@pytest.mark.parametrize("path", PYTHON_PACKAGES, ids=lambda p: p.parent.name)
def test_python_package_names_the_repository(path: Path) -> None:
    project = tomllib.loads(path.read_text(encoding="utf-8"))["project"]
    urls = project.get("urls", {})
    assert urls.get("Homepage") == REPOSITORY
    assert urls.get("Repository") == f"{REPOSITORY}/tree/main/{path.parent.name}"
    assert urls.get("Bug Tracker") == f"{REPOSITORY}/issues"
    assert project.get("license") == {"text": "MIT OR Apache-2.0"}


@pytest.mark.parametrize("path", NODE_PACKAGES, ids=lambda p: p.parent.name)
def test_node_package_names_the_repository(path: Path) -> None:
    package = json.loads(path.read_text(encoding="utf-8"))
    repository = package.get("repository", {})
    url = repository.get("url", "") if isinstance(repository, dict) else repository
    assert url.removeprefix("git+").removesuffix(".git") == REPOSITORY
    assert isinstance(repository, dict) and repository.get("directory") == path.parent.name
    # npm writes an SPDX expression with OR in parentheses.
    assert package.get("license") == "(MIT OR Apache-2.0)"


def test_rust_workspace_names_the_repository() -> None:
    workspace = tomllib.loads((ROOT / "Cargo.toml").read_text(encoding="utf-8"))
    package = workspace["workspace"]["package"]
    assert package["repository"] == REPOSITORY
    assert package["license"] == "MIT OR Apache-2.0"
