"""Every XML reader iterates attributes without quick-xml's duplicate check.

`BytesStart::attributes()` checks each attribute name against every name seen
so far in the same start tag. That is quadratic in the number of attributes
(RUSTSEC-2026-0194): one start tag with 800,000 attributes pins a core for
about ten minutes. `attributes().with_checks(false)` skips the check, and the
advisory names that as unaffected. `try_get_attribute` and `NsReader` reach the
same check internally, so neither may be used.

A duplicated attribute is not well-formed XML. Without the check both copies
are yielded and each reader keeps the one its loop reaches, instead of the
whole tag failing; no reader depended on the failure.
"""

from __future__ import annotations

import re
from pathlib import Path

import pytest

ROOT = Path(__file__).resolve().parents[1]

CHECKED_ITERATION = re.compile(r"\.attributes\(\)(?!\s*\.with_checks\(false\))")
FORBIDDEN = ("try_get_attribute", "NsReader")


def rust_sources() -> list[Path]:
    return sorted(
        path
        for path in ROOT.glob("*/src/**/*.rs")
        if not any(part in {"target", ".claude"} for part in path.parts)
    )


def problems(text: str) -> list[str]:
    # Comments are blanked line by line; the search then runs over the whole
    # text, because rustfmt may put `.with_checks(false)` on the next line.
    code = "\n".join(line.split("//", 1)[0] for line in text.splitlines())

    def line_of(offset: int) -> int:
        return code.count("\n", 0, offset) + 1

    found = [
        f"{line_of(match.start())}: attributes() without with_checks(false)"
        for match in CHECKED_ITERATION.finditer(code)
    ]
    for name in FORBIDDEN:
        found.extend(
            f"{line_of(match.start())}: {name} runs the duplicate check internally"
            for match in re.finditer(re.escape(name), code)
        )
    return found


def test_there_are_sources_to_scan():
    assert any("attributes()" in path.read_text(encoding="utf-8") for path in rust_sources())


def test_no_reader_iterates_attributes_with_the_duplicate_check():
    offenders = {
        str(path.relative_to(ROOT)): found
        for path in rust_sources()
        if (found := problems(path.read_text(encoding="utf-8")))
    }
    assert offenders == {}


@pytest.mark.parametrize(
    "line",
    [
        "for attr in e.attributes().flatten() {",
        "let value = e.try_get_attribute(\"r\")?;",
        "let mut reader = NsReader::from_reader(input);",
        "for attr in e.attributes().with_checks(true) {",
    ],
)
def test_the_scan_catches_each_regression(line: str):
    assert problems(line) != []


def test_the_scan_accepts_the_unchecked_form_and_comments():
    assert problems("for attr in e.attributes().with_checks(false).flatten() {") == []
    assert problems("e.attributes()\n        .with_checks(false)\n        .flatten()") == []
    assert problems("// e.attributes() would run the duplicate check") == []
