"""Every Python binding must be callable by construction, across all crates.

`v2026.09.21` was a fix release for a defect that shipped in `v2026.09.19`:
seven of eight `email_fast` entry points raised `TypeError` on any call, and
`from pdf_fast import *` raised `AttributeError`. Two independent causes, both
of which this module makes impossible to reintroduce in ANY binding crate:

1. **A `#[pyfunction]` with no `#[pyo3(signature = ...)]` makes every parameter
   a required positional argument.** So adding one `Option<T>` retroactively
   makes every existing optional mandatory, silently breaking all callers. The
   one function that survived was the one that already had a signature.
2. **Positional forwarding lets a wrapper misalign.** `parse_mbox_arrow` passed
   `batch_size` into the slot the native function reads as
   `max_total_attachment_bytes`. That raised only while the signatures lacked
   defaults; adding defaults — the fix for cause 1 — would have converted a
   loud failure into a silent one, feeding a batch size to an attachment budget
   indefinitely.

Why this is a static check rather than a call smoke: it needs no built wheel,
no interpreter matching the abi3 tag and no per-format document fixtures, so it
runs in CI on every push at no cost. `email-fast-py/tests/` additionally CALLS
every entry point, which catches things source inspection cannot; the two are
complements. A call smoke is the stronger check and belongs in every crate as
fixtures allow — but a check that runs is worth more than one that is skipped.

**This module derives its subjects from the repository**, listing `*-fast-py`
directories rather than naming crates, because a guard against defects nobody
enumerated cannot itself be a hand-maintained list. A binding added tomorrow is
covered without anyone remembering to add it.
"""

from __future__ import annotations

import re
from pathlib import Path

import pytest

ROOT = Path(__file__).resolve().parents[1]

# There is deliberately NO allowlist of "safe" parameter names here. An earlier
# version had one, and it was the same anti-pattern this module exists to
# prevent: a hand-maintained list of what to check. Which parameters may be
# passed positionally is DERIVED from each native signature — the required ones
# (not `Option<...>`) are positional by nature and their order is part of the
# published API; the optional ones must be forwarded by keyword.


def binding_crates() -> list[Path]:
    """Every `*-fast-py` crate in the workspace, from the filesystem."""
    crates = sorted(p for p in ROOT.glob("*-fast-py") if (p / "src" / "lib.rs").is_file())
    assert crates, "no binding crates found; this guard would pass vacuously"
    return crates


def pyfunctions(lib_rs: str) -> list[tuple[str, bool]]:
    """`(name, has_signature_attribute)` for every #[pyfunction] in a crate."""
    out: list[tuple[str, bool]] = []
    for match in re.finditer(
        r"#\[pyfunction\][^\n]*\n((?:\s*#\[[^\n]*\]\n)*)\s*(?:pub\s+)?fn\s+(\w+)",
        lib_rs,
    ):
        attrs, name = match.group(1), match.group(2)
        out.append((name, "pyo3(signature" in attrs))
    return out


def declared_params(lib_rs: str, fn_name: str) -> list[tuple[str, bool]]:
    """`(name, is_optional)` for each parameter of `fn_name`, in order."""
    start = lib_rs.find(f"fn {fn_name}(")
    if start == -1:
        return []
    body = lib_rs[start + len(f"fn {fn_name}(") : lib_rs.find(")", start)]
    params: list[tuple[str, bool]] = []
    for raw in body.split(","):
        raw = raw.strip()
        if not raw or ":" not in raw:
            continue
        name, _, ty = raw.partition(":")
        name = name.strip()
        if name and name != "py" and not name.startswith("_py"):
            params.append((name, "Option<" in ty))
    return params


def optional_params(lib_rs: str, fn_name: str) -> list[str]:
    """Parameters of `fn_name` declared `Option<...>`."""
    return [name for name, is_opt in declared_params(lib_rs, fn_name) if is_opt]


@pytest.mark.parametrize("crate", binding_crates(), ids=lambda p: p.name)
def test_every_pyfunction_with_optionals_declares_its_defaults(crate: Path) -> None:
    """Cause 1: without a signature attribute, Option<T> params are REQUIRED."""
    lib_rs = (crate / "src" / "lib.rs").read_text(encoding="utf-8")
    offenders = [
        name
        for name, has_sig in pyfunctions(lib_rs)
        if not has_sig and optional_params(lib_rs, name)
    ]
    assert offenders == [], (
        f"{crate.name}: these #[pyfunction]s take Option<T> parameters with no "
        f"#[pyo3(signature = ...)], so every one of them is a REQUIRED positional "
        f"argument and adding another silently breaks all existing callers: {offenders}"
    )


@pytest.mark.parametrize("crate", binding_crates(), ids=lambda p: p.name)
def test_wrappers_forward_optionals_by_keyword(crate: Path) -> None:
    """Cause 2: positional forwarding is what allows a silent misalignment.

    Parsed with ast, not matched with a regex. An earlier version of this test
    scanned line by line and therefore missed every single-line call — it
    reported xlsx_fast's one multi-line call and silently passed its three
    single-line ones, which is the same defect shape the test exists to catch.
    """
    import ast

    package = next((p for p in crate.glob("*_fast") if (p / "__init__.py").is_file()), None)
    if package is None:  # a crate may expose the native module directly
        pytest.skip(f"{crate.name} has no Python wrapper package")
    lib_rs = (crate / "src" / "lib.rs").read_text(encoding="utf-8")
    tree = ast.parse((package / "__init__.py").read_text(encoding="utf-8"))

    offenders: list[str] = []
    for node in ast.walk(tree):
        if not isinstance(node, ast.Call):
            continue
        callee = node.func.id if isinstance(node.func, ast.Name) else None
        if not callee or not callee.endswith("_py"):
            continue
        params = declared_params(lib_rs, callee)
        if not params:
            continue  # not a native function declared in this crate
        for index, arg in enumerate(node.args):
            if index >= len(params):
                continue
            name, is_optional = params[index]
            if is_optional:
                offenders.append(f"{callee}(... {name} ...)")

    assert offenders == [], (
        f"{crate.name}: these arguments are forwarded POSITIONALLY, so reordering "
        f"the native signature would silently change which value lands in which "
        f"parameter: {sorted(set(offenders))}"
    )


@pytest.mark.parametrize("crate", binding_crates(), ids=lambda p: p.name)
def test_all_advertises_only_names_the_wrapper_defines(crate: Path) -> None:
    """`pdf_fast.__all__` advertised a name the module never defined, which
    breaks `from pdf_fast import *` for everyone, exposing nothing."""
    package = next((p for p in crate.glob("*_fast") if (p / "__init__.py").is_file()), None)
    if package is None:
        pytest.skip(f"{crate.name} has no Python wrapper package")
    source = (package / "__init__.py").read_text(encoding="utf-8")

    block = re.search(r"__all__\s*=\s*[\[\(](.*?)[\]\)]", source, re.DOTALL)
    if block is None:
        pytest.skip(f"{crate.name} declares no __all__")
    advertised = set(re.findall(r'"([^"]+)"', block.group(1)))

    # Parse the module properly rather than by regex: a multi-line
    # `from ._core import (a, b, c)` is the normal way a binding re-exports its
    # native names, and a regex that misses it reports false defects — as an
    # earlier version of this test did, for pdf_fast and json_fast, whose names
    # all resolve at runtime.
    import ast

    tree = ast.parse(source)
    defined: set[str] = set()
    for node in ast.walk(tree):
        if isinstance(node, (ast.FunctionDef, ast.AsyncFunctionDef, ast.ClassDef)):
            defined.add(node.name)
        elif isinstance(node, ast.ImportFrom):
            for alias in node.names:
                defined.add(alias.asname or alias.name)
        elif isinstance(node, ast.Import):
            for alias in node.names:
                defined.add(alias.asname or alias.name.split(".")[0])
        elif isinstance(node, ast.Assign):
            for target in node.targets:
                if isinstance(target, ast.Name):
                    defined.add(target.id)
        elif isinstance(node, ast.AnnAssign) and isinstance(node.target, ast.Name):
            defined.add(node.target.id)

    missing = sorted(advertised - defined)
    assert missing == [], (
        f"{crate.name}: __all__ advertises names the wrapper does not define, so "
        f"`from {package.name} import *` raises AttributeError: {missing}"
    )
