"""Dependencies that parse untrusted input are built without optional unsafe
code this workspace does not use."""

from __future__ import annotations

import tomllib
from pathlib import Path

import pytest

ROOT = Path(__file__).resolve().parents[1]
MANIFESTS = sorted(ROOT.glob("*/Cargo.toml"))
DEPENDENCY_TABLES = ("dependencies", "dev-dependencies", "build-dependencies")


def base64_requirements(manifest: dict) -> list[object]:
    """Every `base64` requirement in a parsed manifest, target tables included."""
    tables = [manifest]
    tables.extend(manifest.get("target", {}).values())
    return [
        table[kind]["base64"]
        for table in tables
        for kind in DEPENDENCY_TABLES
        if "base64" in table.get(kind, {})
    ]


def assert_base64_without_simd(manifest: dict) -> None:
    # base64 0.23 turns on SIMD engines written in unsafe code by default.
    # The crates here decode attacker-supplied text with the general-purpose
    # engine only, so the unsafe code stays out of the build: without
    # `simd-unsafe` base64 is `#![forbid(unsafe_code)]`.
    for requirement in base64_requirements(manifest):
        assert isinstance(requirement, dict), "base64 must name its features"
        assert requirement.get("default-features") is False, (
            "base64 must not take its default features (they enable simd-unsafe)"
        )
        assert "simd-unsafe" not in requirement.get("features", [])
        assert requirement.get("workspace") is not True


def test_the_manifests_that_use_base64_are_found() -> None:
    users = [
        path.parent.name
        for path in MANIFESTS
        if base64_requirements(tomllib.loads(path.read_text(encoding="utf-8")))
    ]
    assert {"email-fast-core", "json-fast-core", "pdf-fast-core"} <= set(users)


@pytest.mark.parametrize("path", MANIFESTS, ids=lambda p: p.parent.name)
def test_base64_is_built_without_its_unsafe_simd_engines(path: Path) -> None:
    assert_base64_without_simd(tomllib.loads(path.read_text(encoding="utf-8")))


@pytest.mark.parametrize(
    "requirement",
    [
        '"0.23"',
        '{ version = "0.23" }',
        '{ version = "0.23", features = ["std"] }',
        '{ version = "0.23", default-features = false, features = ["std", "simd-unsafe"] }',
    ],
)
def test_the_check_refuses_a_build_with_simd(requirement: str) -> None:
    for table in ("dependencies", 'target."cfg(unix)".dependencies'):
        manifest = tomllib.loads(f"[{table}]\nbase64 = {requirement}\n")
        with pytest.raises(AssertionError):
            assert_base64_without_simd(manifest)
