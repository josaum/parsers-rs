from __future__ import annotations

import io
import json
import sys
import tarfile
import zipfile
from pathlib import Path

import pytest

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT / "scripts"))

import check_license_files  # noqa: E402

LICENSES = check_license_files.expected_texts(ROOT)


def write_wheel(path: Path, licenses: dict[str, bytes]) -> None:
    with zipfile.ZipFile(path, "w") as archive:
        archive.writestr("demo_fast-0.1.0.dist-info/METADATA", "Name: demo-fast\n")
        for name, text in licenses.items():
            archive.writestr(f"demo_fast-0.1.0.dist-info/licenses/{name}", text)


def write_tarball(path: Path, licenses: dict[str, bytes]) -> None:
    with tarfile.open(path, "w:gz") as archive:
        for name, text in {"package.json": b"{}", **licenses}.items():
            info = tarfile.TarInfo(f"package/{name}")
            info.size = len(text)
            archive.addfile(info, io.BytesIO(text))


def test_artifacts_with_both_texts_pass(tmp_path: Path):
    write_wheel(tmp_path / "demo_fast-0.1.0-cp38-abi3-any.whl", LICENSES)
    write_tarball(tmp_path / "demo-fast-node-0.1.0-linux-x64-gnu.tgz", LICENSES)
    assert check_license_files.check_dist(tmp_path, ROOT) == []
    assert check_license_files.main(["--dist", str(tmp_path)]) == 0


@pytest.mark.parametrize("missing", check_license_files.LICENSE_FILES)
def test_wheel_missing_either_text_fails(tmp_path: Path, missing: str):
    kept = {name: text for name, text in LICENSES.items() if name != missing}
    write_wheel(tmp_path / "demo_fast-0.1.0-cp38-abi3-any.whl", kept)
    problems = check_license_files.check_dist(tmp_path, ROOT)
    assert problems == [
        "demo_fast-0.1.0-cp38-abi3-any.whl: missing "
        f"demo_fast-0.1.0.dist-info/licenses/{missing}"
    ]
    assert check_license_files.main(["--dist", str(tmp_path)]) == 1


@pytest.mark.parametrize("missing", check_license_files.LICENSE_FILES)
def test_tarball_missing_either_text_fails(tmp_path: Path, missing: str):
    kept = {name: text for name, text in LICENSES.items() if name != missing}
    write_tarball(tmp_path / "demo-fast-node-0.1.0-linux-x64-gnu.tgz", kept)
    assert check_license_files.check_dist(tmp_path, ROOT) == [
        f"demo-fast-node-0.1.0-linux-x64-gnu.tgz: missing package/{missing}"
    ]


def test_altered_text_fails(tmp_path: Path):
    altered = {**LICENSES, "LICENSE": LICENSES["LICENSE"] + b"extra\n"}
    write_wheel(tmp_path / "demo_fast-0.1.0-cp38-abi3-any.whl", altered)
    problems = check_license_files.check_dist(tmp_path, ROOT)
    assert len(problems) == 1 and "differs from the root LICENSE" in problems[0]


def test_empty_dist_fails(tmp_path: Path):
    assert check_license_files.check_dist(tmp_path, ROOT) != []


def test_every_binding_directory_carries_both_root_texts():
    bindings = sorted(
        path for path in ROOT.iterdir()
        if path.is_dir() and path.name.endswith(("-py", "-node"))
    )
    assert bindings, "no binding directories found"
    for binding in bindings:
        for name, text in LICENSES.items():
            copy = binding / name
            assert copy.is_file(), f"{binding.name} is missing {name}"
            assert copy.read_bytes() == text, f"{binding.name}/{name} is stale"


def test_node_packages_publish_both_texts_and_declare_the_dual_license():
    for manifest in sorted(ROOT.glob("*-node/package.json")):
        package = json.loads(manifest.read_text(encoding="utf-8"))
        assert package["license"] == "(MIT OR Apache-2.0)", manifest
        for name in check_license_files.LICENSE_FILES:
            assert name in package["files"], f"{manifest}: files omits {name}"


def test_release_workflow_checks_and_publishes_the_license_texts():
    workflow = (ROOT / ".github/workflows/release.yml").read_text(encoding="utf-8")
    assert "python3 scripts/check_license_files.py --dist dist" in workflow
    # The check must run after the SBOM injector rewrites the wheels and before
    # the manifest stamps their hashes.
    assert (
        workflow.index("scripts/inject_sbom.py --dist dist")
        < workflow.index("scripts/check_license_files.py --dist dist")
        < workflow.index("scripts/release_manifest.py --out dist")
    )
    for block in workflow.split("files: |")[1:2] + workflow.split("path: |")[-1:]:
        assert "LICENSE\n" in block and "LICENSE-APACHE\n" in block
