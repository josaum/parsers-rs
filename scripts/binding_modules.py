#!/usr/bin/env python3
"""Print the import name of each Python binding crate named on the command line.

The import name is the top-level package of `[tool.maturin] module-name` in
the crate's pyproject.toml (`docx_fast._core` gives `docx_fast`). It is read,
not derived from the crate name, because a package may import under another
name: `mcts-fast-py` imports as `mcts_fast_py`.
"""

from __future__ import annotations

import sys
import tomllib
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]


def import_name(crate: str) -> str:
    """The top-level package a binding crate's wheel installs."""
    project = tomllib.loads((ROOT / crate / "pyproject.toml").read_text(encoding="utf-8"))
    return project["tool"]["maturin"]["module-name"].split(".")[0]


if __name__ == "__main__":
    print(" ".join(import_name(crate) for crate in sys.argv[1:]))
