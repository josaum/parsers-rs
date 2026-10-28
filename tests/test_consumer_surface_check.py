from __future__ import annotations

import importlib.util
from pathlib import Path, PureWindowsPath

import pytest

ROOT = Path(__file__).resolve().parents[1]


def load_module():
    path = ROOT / "scripts" / "consumer_surface_check.py"
    spec = importlib.util.spec_from_file_location("consumer_surface_check", path)
    assert spec and spec.loader
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


@pytest.mark.parametrize(
    ("platform", "expected_wheel"),
    [
        ("win-amd64", "pdf_fast-0.2.5-cp312-abi3-win_amd64.whl"),
        ("win_arm64", "pdf_fast-0.2.5-cp312-abi3-win_arm64.whl"),
        ("macosx-11.0-arm64", "pdf_fast-0.2.5-cp312-abi3-macosx_11_0_arm64.whl"),
        ("linux-x86_64", "pdf_fast-0.2.5-cp312-abi3-manylinux_2_17_x86_64.whl"),
    ],
)
def test_platform_wheels_selects_windows_and_posix_wheels(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch, platform: str, expected_wheel: str
) -> None:
    mod = load_module()
    for name in (
        "pdf_fast-0.2.5-cp312-abi3-win_amd64.whl",
        "pdf_fast-0.2.5-cp312-abi3-win_arm64.whl",
        "pdf_fast-0.2.5-cp312-abi3-macosx_11_0_arm64.whl",
        "pdf_fast-0.2.5-cp312-abi3-manylinux_2_17_x86_64.whl",
    ):
        (tmp_path / name).write_bytes(b"")

    monkeypatch.setattr(mod.sysconfig, "get_platform", lambda: platform)
    selected = mod.platform_wheels(tmp_path)
    assert [w.name for w in selected] == [expected_wheel]


def test_venv_binaries_use_scripts_on_windows(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    mod = load_module()
    monkeypatch.setattr(mod.sys, "platform", "win32")
    assert mod.venv_python(tmp_path) == tmp_path / "Scripts" / "python.exe"
    assert mod.venv_pip(tmp_path) == tmp_path / "Scripts" / "pip.exe"

    monkeypatch.setattr(mod.sys, "platform", "linux")
    assert mod.venv_python(tmp_path) == tmp_path / "bin" / "python"
    assert mod.venv_pip(tmp_path) == tmp_path / "bin" / "pip"


def test_collect_references_handles_utf8_and_skips_ignored_dirs(tmp_path: Path) -> None:
    mod = load_module()
    (tmp_path / "src").mkdir()
    (tmp_path / "src" / "use.py").write_text(
        "# UTF-8 comment: ── × ² → …\nimport pdf_fast\npdf_fast.parse_pdf('x.pdf')\n",
        encoding="utf-8",
    )
    for ignored in (".venv", "node_modules", "target", "__pycache__"):
        d = tmp_path / ignored
        d.mkdir()
        (d / "skip.py").write_text("pdf_fast.should_not_appear()\n", encoding="utf-8")

    refs = mod.collect_references([tmp_path], exclude=[], packages={"pdf_fast"})
    assert set(refs["pdf_fast"].keys()) == {"parse_pdf"}
    assert PureWindowsPath("C:\\repo\\.venv\\lib\\x.py").as_posix().endswith(
        "/.venv/lib/x.py"
    )
