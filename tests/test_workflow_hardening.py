"""Supply-chain and least-privilege invariants of the GitHub Actions workflows.

The workflows are read as text, like the other workflow tests: the suite runs
with nothing but pytest installed, and a version comment after a pinned action
is invisible to a YAML parser anyway. Every helper below therefore relies on the
block style these files are written in (two-space indentation, one key per
line), and each contract refuses a spelling it cannot read.
"""

from __future__ import annotations

import re
import tomllib
from pathlib import Path

import pytest

ROOT = Path(__file__).resolve().parents[1]
WORKFLOWS = ROOT / ".github/workflows"
CI = WORKFLOWS / "ci.yml"
RELEASE = WORKFLOWS / "release.yml"
AUDIT = WORKFLOWS / "audit.yml"
FUZZ = WORKFLOWS / "fuzz.yml"
DEPENDABOT = ROOT / ".github/dependabot.yml"
ALL_WORKFLOWS = sorted([*WORKFLOWS.glob("*.yml"), *WORKFLOWS.glob("*.yaml")])

HOSTED_RUNNERS = {"ubuntu-latest", "macos-latest", "windows-latest"}
# The one job in each workflow that may hold a token able to write.
WRITERS = {"release.yml": {"manifest": {"contents": "write"}}}
# Actions that publish no version tags: the comment names the branch whose
# history holds the pinned commit.
BRANCH_PINNED = {"dtolnay/rust-toolchain": "master"}
# The only ci.yml jobs that may be skipped, each mapped to the release.yml job
# that does the same work again. They are skipped only when release.yml calls
# ci.yml with SKIP_INPUT set; a push or a pull request has no inputs, and the
# condition must read an absent input as "run".
REBUILT_BY_RELEASE = {"python-bindings": "wheels", "node-bindings": "node"}
SKIP_INPUT = "skip_binding_jobs"
# The doctors are a package outside the workspace; this is its own lockfile.
DOCTORS_LOCK = ".leio-code/rust-doctors/Cargo.lock"
DOCTORS_MANIFEST = ".leio-code/rust-doctors/Cargo.toml"
# The checks branch protection requires, as ci.yml job names. A job with a
# matrix reports once per leg, so these seven jobs are the twelve required
# checks. Renaming or removing one leaves its check pending on every pull
# request; a job that is not listed here is not required.
REQUIRED_CI_JOBS = {
    "rust": None,
    "rust-platforms": {"macos-latest", "windows-latest"},
    "python-bindings": HOSTED_RUNNERS,
    "node-bindings": HOSTED_RUNNERS,
    "release-tooling": None,
    "repository-doctors": None,
    "lean-formal": None,
}
# ci.yml jobs that report but are not required checks.
OPTIONAL_CI_JOBS = {"rustdoc"}
DENY_CONFIG = "deny.toml"
SKIP_CONDITION = "${{ inputs.skip_binding_jobs != true }}"

USES = re.compile(r"^\s*(?:- )?uses:[ ]+(?P<ref>\S+)(?P<rest>.*)$")
PINNED = re.compile(
    r"(?P<action>[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+)(?:/[A-Za-z0-9_./-]+)?@[0-9a-f]{40}"
)


def read(path: Path) -> str:
    return path.read_text(encoding="utf-8")


def code(text: str) -> str:
    """The text without its whole-line comments, so prose cannot satisfy a check."""
    return "".join(
        line for line in text.splitlines(keepends=True) if not line.lstrip().startswith("#")
    )


def top_level(text: str, key: str) -> str | None:
    """Everything under a top-level key: its inline value and its indented block."""
    found = re.search(
        rf"^{re.escape(key)}:(?P<body>[^\n]*\n(?:[ ]+\S[^\n]*\n|[ ]*\n)*)",
        code(text),
        re.MULTILINE,
    )
    return found["body"] if found else None


def jobs(text: str) -> dict[str, str]:
    """Each job's name mapped to the text of its definition."""
    body = top_level(text, "jobs")
    assert body is not None, "workflow has no jobs"
    parts = re.split(r"^  ([A-Za-z_][A-Za-z0-9_-]*):[ ]*\n", body, flags=re.MULTILINE)
    found = dict(zip(parts[1::2], parts[2::2]))
    assert found, "workflow has no jobs"
    return found


def required_jobs(text: str, *names: str) -> dict[str, str]:
    """The workflow's jobs, after checking that the named ones exist."""
    found = jobs(text)
    for name in names:
        assert name in found, f"workflow has no `{name}` job"
    return found


def job_value(block: str, key: str) -> str | None:
    """The inline value of a key set directly on a job."""
    found = re.search(rf"^    {re.escape(key)}:[ ]*(.*)$", block, re.MULTILINE)
    return found[1] if found else None


def mapping(text: str, key: str, indent: int) -> dict[str, str] | None:
    """A block mapping of scalars under `key`, which sits at `indent` spaces."""
    pad = " " * indent
    found = re.search(
        rf"^{pad}{re.escape(key)}:[ ]*\n(?P<body>(?:{pad}  [^\n]*\n)+)",
        text,
        re.MULTILINE,
    )
    if found is None:
        return None
    pairs = (line.strip().partition(":") for line in found["body"].splitlines())
    return {name.strip(): value.strip() for name, _, value in pairs}


def runner_labels(block: str) -> set[str]:
    """Every label a job can run on, following a plain `matrix.os` list."""
    value = job_value(block, "runs-on")
    assert value, "job names no runner"
    if value != "${{ matrix.os }}":
        return {value}
    row = re.search(r"^        os: \[(?P<labels>[^\]\n]+)\]$", block, re.MULTILINE)
    assert row is not None, "matrix.os is not a plain list of runner labels"
    return {label.strip() for label in row["labels"].split(",")}


def assert_actions_pinned(text: str) -> None:
    seen = 0
    for line in code(text).splitlines():
        if "uses:" not in line:
            continue
        step = USES.match(line)
        assert step is not None, f"cannot read this `uses`: {line.strip()}"
        seen += 1
        ref = step["ref"]
        if ref.startswith("./"):
            assert (ROOT / ref).is_file(), f"{ref}: local workflow or action is missing"
            continue
        pinned = PINNED.fullmatch(ref)
        assert pinned is not None, f"{ref}: not pinned by a full 40-character commit SHA"
        expected = BRANCH_PINNED.get(pinned["action"])
        comment = rf" # {re.escape(expected)}" if expected else r" # v\d+\.\d+\.\d+"
        assert re.fullmatch(comment, step["rest"]), (
            f"{ref}: the pin must be followed by a comment naming its version"
        )
    assert seen, "workflow uses no action at all"


def assert_checkouts_keep_no_credentials(text: str) -> None:
    """No job pushes, so no checkout may leave the token in .git/config."""
    steps = re.split(r"^      - ", code(text), flags=re.MULTILINE)
    checkouts = [step for step in steps if step.startswith("uses: actions/checkout@")]
    assert len(checkouts) == code(text).count("actions/checkout@"), "a checkout in a spelling this cannot read"
    assert checkouts, "workflow checks nothing out"
    for step in checkouts:
        assert mapping(step, "with", 8) == {"persist-credentials": "false"}, (
            "every checkout must set persist-credentials: false and nothing else"
        )


DOWNLOAD = re.compile(
    r'curl -sSfL -o "\$RUNNER_TEMP/(?P<file>[\w.-]+)" \\\n\s+(?P<url>https://\S+)\n'
    r'\s+echo "[0-9a-f]{64}  \$RUNNER_TEMP/(?P=file)" \\\n\s+\| sha256sum --check --strict\n'
)
# A download names exactly what it fetches: a file at a full commit, or one
# asset of one numbered release.
IMMUTABLE_URL = re.compile(
    r"https://raw\.githubusercontent\.com/[\w.-]+/[\w.-]+/[0-9a-f]{40}/\S+"
    r"|https://github\.com/[\w.-]+/[\w.-]+/releases/download/v\d+\.\d+\.\d+/[\w.-]+"
)


def assert_installers_are_pinned(text: str) -> None:
    """Nothing fetched from the network runs unchecked, and no tool floats.

    The only download spelling accepted is `curl` writing a file under
    $RUNNER_TEMP from an immutable URL, with the very next command checking
    that file against a SHA-256. Every other way of fetching, or of running
    fetched bytes, is refused outright instead of being recognized case by case.
    """
    body = code(text)
    accepted = list(DOWNLOAD.finditer(body))
    assert len(re.findall(r"\bcurl\b", body)) == len(accepted), (
        "every curl must write a file and be followed at once by its SHA-256 check"
    )
    for download in accepted:
        assert IMMUTABLE_URL.fullmatch(download["url"]), (
            f"{download['url']}: not a commit-pinned file or a versioned release asset"
        )
    for fetcher in ("wget", "Invoke-WebRequest", "iwr ", "irm "):
        assert fetcher not in body, f"{fetcher.strip()}: downloads go through the checked curl spelling"
    # Fetched bytes reaching an interpreter without touching the checked file.
    assert not re.search(r"\|\s*(?:sudo\s+)?(?:ba|z|da|k)?sh\b", body), "something is piped into a shell"
    assert not re.search(r"\|\s*(?:python3?|node|perl|ruby|pwsh|iex)\b", body), (
        "something is piped into an interpreter"
    )
    assert "<(" not in body, "process substitution can run a download unchecked"
    assert not re.search(r"\beval\b|\bsource\s+/dev/stdin", body), (
        "a command built from output can run a download unchecked"
    )
    for install in re.findall(r"cargo install [^\n]+", body):
        assert re.search(r" --locked\b", install) and re.search(
            r" --version [0-9]+\.[0-9]+\.[0-9]+(?:\s|$)", install
        ), f"`{install}`: not pinned to an exact version"


def assert_ci_runs_on_every_push_and_pull_request(ci: str) -> None:
    """ci.yml's jobs are required checks: no trigger filter may leave one unreported."""
    triggers = top_level(ci, "on") or ""
    # A pull request with no qualifier at all, and pushes to main.
    assert re.search(r"^  pull_request:\n(?!    )", triggers, re.MULTILINE), (
        "pull_request must carry no filter: a filtered run leaves required checks pending"
    )
    assert re.search(r"^  push:\n    branches: \[main\]\n(?!    )", triggers, re.MULTILINE), (
        "push must run for every commit on main"
    )
    for filtered in ("paths", "paths-ignore", "branches-ignore", "tags-ignore", "types"):
        assert not re.search(rf"^\s+{filtered}:", triggers, re.MULTILINE), (
            f"ci.yml must not filter its triggers with `{filtered}`"
        )
    assert len(re.findall(r"^\s+branches:", triggers, re.MULTILINE)) == 1


def assert_toolchain_named(text: str) -> None:
    """A SHA-pinned dtolnay/rust-toolchain installs nothing unless told what."""
    channel = re.search(r'^channel = "([^"]+)"$', read(ROOT / "rust-toolchain.toml"), re.MULTILINE)
    assert channel is not None
    for step in re.split(r"^      - ", code(text), flags=re.MULTILINE):
        if "uses: dtolnay/rust-toolchain@" not in step:
            continue
        assert re.search(
            rf"^          toolchain: {re.escape(channel[1])}$", step, re.MULTILINE
        ), f"dtolnay/rust-toolchain step does not install {channel[1]}"


def assert_least_privilege(text: str, writers: dict[str, dict[str, str]] | None = None) -> None:
    body = code(text)
    assert mapping(body, "permissions", 0) == {"contents": "read"}, (
        "the workflow's default token must be `contents: read` and nothing else"
    )
    granted = {
        name: scopes
        for name, block in jobs(text).items()
        if (scopes := mapping(block, "permissions", 4)) is not None
    }
    writing = {
        name: scopes
        for name, scopes in granted.items()
        if any(level not in ("read", "none") for level in scopes.values())
    }
    assert writing == (writers or {}), f"jobs holding a write token: {sorted(writing)}"
    # Nothing is granted in a spelling the lines above cannot read
    # (`permissions: write-all`, a flow mapping, another indentation).
    assert not re.search(r"^\s*permissions:[ ]*\S", body, re.MULTILINE)
    assert len(re.findall(r"^\s*permissions:", body, re.MULTILINE)) == 1 + len(granted)


def assert_release_token_scopes(release: str) -> None:
    assert_least_privilege(release, WRITERS["release.yml"])
    blocks = required_jobs(release, "verify", "manifest")
    publishers = [name for name, block in blocks.items() if "softprops/action-gh-release@" in block]
    assert publishers == ["manifest"], "only the manifest job publishes the release"
    assert "self-hosted" not in blocks["manifest"], "the publishing job must run on a hosted runner"
    verify = blocks["verify"]
    assert job_value(verify, "uses") == "./.github/workflows/ci.yml"
    assert mapping(verify, "permissions", 4) == {"contents": "read"}
    assert job_value(verify, "secrets") is None, "ci.yml must not be handed secrets"


def assert_every_job_has_a_timeout(text: str) -> None:
    for name, block in jobs(text).items():
        timeout = job_value(block, "timeout-minutes")
        if job_value(block, "uses") is not None:
            # GitHub rejects a timeout on a job that calls a reusable workflow;
            # the called workflow's own jobs are checked like any other.
            assert timeout is None, f"{name}: a reusable-workflow call cannot carry a timeout"
            continue
        assert timeout is not None, f"{name}: no timeout-minutes"
        assert re.fullmatch(r"[1-9][0-9]*", timeout), f"{name}: timeout is not a number"
        assert int(timeout) < 360, f"{name}: a timeout at GitHub's default is no timeout"


def assert_ci_concurrency(ci: str) -> None:
    group = mapping(code(ci), "concurrency", 0)
    assert group is not None, "ci.yml has no concurrency group"
    # Keyed on the pull request, so a newer push replaces the run before it;
    # every other trigger keys on its own run and can never be cancelled.
    assert "github.event.pull_request.number" in group["group"]
    assert "github.run_id" in group["group"]
    assert group["cancel-in-progress"] == "${{ github.event_name == 'pull_request' }}"


def assert_hosted_runners_only(text: str) -> None:
    assert "self-hosted" not in code(text).lower()
    assert "pull_request_target" not in code(text)
    for name, block in jobs(text).items():
        labels = runner_labels(block)
        assert labels <= HOSTED_RUNNERS, f"{name}: not a GitHub-hosted runner: {sorted(labels)}"


def assert_ci_needs_no_secret(ci: str) -> None:
    body = code(ci)
    for forbidden in ("secrets.", "secrets:", "github.token", "GITHUB_TOKEN"):
        assert forbidden not in body, f"ci.yml must not use {forbidden}"


def assert_ci_exercises_release_paths(ci: str, release: str | None = None) -> None:
    release = read(RELEASE) if release is None else release
    blocks = required_jobs(ci, "rust-platforms", "python-bindings", "node-bindings")
    # The binding jobs are bash scripts and also run on Windows, where the
    # default shell is PowerShell.
    for name in ("python-bindings", "node-bindings"):
        assert "    defaults:\n      run:\n        shell: bash\n" in blocks[name], (
            f"{name}: runs on Windows without `shell: bash` as its default"
        )

    platforms = blocks["rust-platforms"]
    assert runner_labels(platforms) == {"macos-latest", "windows-latest"}
    assert "      - run: cargo test --workspace --locked --no-fail-fast\n" in platforms
    assert (
        "      - run: cargo test --workspace --locked --no-fail-fast"
        " --features ${{ env.ARROW_FEATURES }}\n"
    ) in platforms

    python = blocks["python-bindings"]
    assert runner_labels(python) == HOSTED_RUNNERS
    tested = re.search(r"^      TESTED_BINDINGS: (.+)$", python, re.MULTILINE)
    assert tested is not None
    crates = tested[1].split()
    with_tests = sorted(path.parent.name for path in ROOT.glob("*-py/tests") if path.is_dir())
    assert sorted(crates) == with_tests, "CI must test every Python binding that has tests"
    smoke = re.search(
        r"^      - name: Smoke native ARM wheel artifacts\n(?P<body>.*?)(?=^      - |\Z)",
        release,
        re.MULTILINE | re.DOTALL,
    )
    assert smoke is not None
    assert sorted(re.findall(r"(\S+-py)/tests", smoke["body"])) == with_tests

    def pins(text: str) -> set[tuple[str, str]]:
        return set(re.findall(r'"(maturin|pytest|pyarrow)==([^"]+)"', text))

    assert {tool for tool, _ in pins(python)} == {"maturin", "pytest", "pyarrow"}
    assert pins(python) == pins(release), "CI and the release must build and test with the same tools"
    assert 'python-version: "3.12"' in python
    assert "maturin build --release --locked" in python
    assert "for crate in $TESTED_BINDINGS" in python
    assert "python -m pip install --no-index --no-deps dist/*.whl" in python
    assert "importlib.import_module" in python
    assert 'python -I -m pytest --import-mode=importlib -q "${test_paths[@]}"' in python
    assert 'test_paths+=("$crate/tests")' in python

    node = blocks["node-bindings"]
    assert runner_labels(node) == HOSTED_RUNNERS
    assert "        run: scripts/build-node.sh\n" in node
    assert "        run: node scripts/smoke-node.mjs\n" in node
    assert node.index("scripts/build-node.sh") < node.index("node scripts/smoke-node.mjs")
    assert "        run: node scripts/check-node-types.mjs\n" in node
    assert node.index("scripts/build-node.sh") < node.index("node scripts/check-node-types.mjs")
    # Looping over `*-node` would also pick up dist-node, the pack destination.
    assert "for manifest in *-node/package.json; do" in node
    assert "pnpm pack --pack-destination ../dist-node" in node
    tools = r'pnpm/action-setup@[^\n]+\n +with:\n +version: (\S+)\n.*?node-version: ("[^"\n]+")'
    ci_tools = re.search(tools, node, re.DOTALL)
    release_tools = re.search(tools, release, re.DOTALL)
    assert ci_tools is not None and release_tools is not None
    assert ci_tools.groups() == release_tools.groups() == ("10", '"24"')


def assert_ci_skips_only_what_the_release_rebuilds(ci: str) -> None:
    """No ci.yml job can be skipped, except the two binding jobs by the release."""
    triggers = top_level(ci, "on") or ""
    assert re.search(r"^  workflow_call:$", triggers, re.MULTILINE)
    # `inputs` is one context for workflow_call and workflow_dispatch: a manual
    # trigger declaring the same name could skip the jobs outside a release.
    assert set(re.findall(r"^  ([A-Za-z_]+):", triggers, re.MULTILINE)) == {
        "push",
        "pull_request",
        "workflow_call",
    }, "ci.yml runs on push, on pull requests and when called; nothing else may set an input"
    declared = re.search(
        r"^  workflow_call:\n    inputs:\n(?P<body>(?:      [^\n]*\n)+)", triggers, re.MULTILINE
    )
    assert declared is not None, "ci.yml declares no workflow_call inputs"
    names = re.findall(r"^      (\S+):[ ]*$", declared["body"], re.MULTILINE)
    assert names == [SKIP_INPUT], f"ci.yml may take exactly one input, {SKIP_INPUT}"
    spec = mapping(declared["body"], SKIP_INPUT, 6)
    assert spec is not None
    assert set(spec) == {"description", "type", "required", "default"}
    # Absent means "run": a caller that says nothing skips nothing.
    assert (spec["type"], spec["required"], spec["default"]) == ("boolean", "false", "false")

    blocks = required_jobs(ci, *REBUILT_BY_RELEASE)
    for name, block in blocks.items():
        condition = job_value(block, "if")
        if name in REBUILT_BY_RELEASE:
            # `!= true` holds for false and for the empty value a push or a
            # pull request sees; `== false` or a bare negation is not accepted.
            assert condition == SKIP_CONDITION, (
                f"ci.yml job {name} must run unless the release sets {SKIP_INPUT}"
            )
        else:
            assert condition is None, f"ci.yml job {name} can be skipped"
        # A job that is allowed to fail would let a release through.
        assert "continue-on-error" not in block, f"ci.yml job {name} can fail silently"
    # Nothing is skipped in a spelling the loop above cannot read (a step-level
    # `if`, another indentation), and the input steers nothing else.
    body = code(ci)
    assert len(re.findall(r"^\s*(?:- )?if:", body, re.MULTILINE)) == len(REBUILT_BY_RELEASE)
    assert body.count("inputs.") == len(REBUILT_BY_RELEASE)


def assert_release_tag_is_on_main(blocks: dict[str, str]) -> None:
    """A tag is released only if its commit is on main, where CI ran in full."""
    gate = blocks["on-main"]
    # Everything else waits for it, through `verify`.
    assert job_value(blocks["verify"], "needs") == "on-main"
    assert job_value(gate, "needs") is None and job_value(gate, "if") is None
    assert "continue-on-error" not in gate
    assert not re.search(r"^\s*(?:- )?if:", gate, re.MULTILINE), "the check itself can be skipped"
    assert "self-hosted" not in gate and job_value(gate, "runs-on") == "ubuntu-latest"
    script = re.search(r"^        run: \|\n(?P<body>(?:          [^\n]*\n|\n)+)", gate, re.MULTILINE)
    assert script is not None
    lines = [line.strip() for line in script["body"].splitlines() if line.strip()]
    lines = [line for line in lines if not line.startswith("#")]
    assert lines[0] == "set -euo pipefail"
    # Only a ref that is not a release tag leaves early; a manual trial
    # publishes nothing (the publish step has the same condition).
    assert lines[1:5] == [
        'case "$GITHUB_REF" in',
        "refs/tags/v*) ;;",
        '*) echo "$GITHUB_REF is not a release tag; nothing will be published."; exit 0 ;;',
        "esac",
    ]
    assert lines[5:] == [
        "git fetch --no-tags --depth=2147483647 origin '+refs/heads/main:refs/remotes/origin/main'",
        'git merge-base --is-ancestor "$GITHUB_SHA" origin/main',
    ]
    assert "if: startsWith(github.ref, 'refs/tags/v')" in blocks["manifest"]


def assert_every_wheel_leg_imports_its_wheels(wheels: str) -> None:
    """No leg uploads a wheel that nothing installed and imported."""
    steps = re.split(r"^      - ", wheels, flags=re.MULTILINE)
    names = [step.split("\n", 1)[0] for step in steps]
    smoke = [i for i, step in enumerate(steps) if step.startswith("name: Install and import every wheel")]
    arm = [i for i, step in enumerate(steps) if step.startswith("name: Smoke native ARM wheel artifacts")]
    upload = [i for i, name in enumerate(names) if name.startswith("uses: actions/upload-artifact@")]
    assert len(smoke) == len(arm) == len(upload) == 1
    assert smoke[0] < upload[0] and arm[0] < upload[0], "wheels are uploaded before they are imported"
    step = steps[smoke[0]]
    # Between them the two smokes cover every leg: this one is every target
    # but ARM Linux, the other one is ARM Linux.
    conditions = re.findall(r"^        if: (.+)$", step, re.MULTILINE)
    assert conditions == ["matrix.target != 'aarch64-unknown-linux-gnu'"]
    assert re.findall(r"^        if: (.+)$", steps[arm[0]], re.MULTILINE) == [
        "matrix.target == 'aarch64-unknown-linux-gnu'"
    ]
    assert "continue-on-error" not in wheels
    assert "set -euo pipefail" in step
    assert 'test "${#built_wheels[@]}" -eq "${#built_bindings[@]}"' in step
    assert "python -m pip install --no-index --no-deps dist/*.whl" in step
    assert "python -I -c" in step and "importlib.import_module" in step
    assert "os.environ['BINDINGS'].split()" in step


def assert_release_waits_for_ci(release: str, ci: str | None = None) -> None:
    ci = read(CI) if ci is None else ci
    blocks = required_jobs(release, "on-main", "verify", "wheels", "node", "manifest")
    verify = blocks["verify"]
    assert job_value(verify, "uses") == "./.github/workflows/ci.yml"
    assert job_value(verify, "if") is None, "the CI gate must never be skipped"
    assert_release_tag_is_on_main(blocks)
    assert_every_wheel_leg_imports_its_wheels(blocks["wheels"])
    assert job_value(blocks["wheels"], "needs") == "verify"
    assert job_value(blocks["node"], "needs") == "verify"
    assert job_value(blocks["manifest"], "needs") == "[wheels, node]"
    assert_ci_skips_only_what_the_release_rebuilds(ci)
    # The release leaves the two binding jobs out of the gate, and only those:
    # it passes the one input ci.yml takes and nothing else.
    assert mapping(verify, "with", 4) == {SKIP_INPUT: "true"}
    # A skipped job is acceptable only while the release job that replaces it
    # still does the work, and while the release cannot finish without it.
    assert set(REBUILT_BY_RELEASE.values()) == {"wheels", "node"}
    for job in REBUILT_BY_RELEASE.values():
        assert job_value(blocks[job], "if") is None, f"release job {job} can be skipped"
        assert "continue-on-error" not in blocks[job], f"release job {job} can fail silently"
    assert blocks["wheels"].count("maturin build --release --locked") == 2
    assert blocks["wheels"].count("for crate in $BINDINGS") == 2
    node = blocks["node"]
    assert 'scripts/build-node.sh "${node_bindings[@]}"' in node
    assert "        run: node scripts/smoke-node.mjs\n" in node
    assert "        run: node scripts/check-node-types.mjs\n" in node
    assert "pnpm pack --pack-destination ../dist-node" in node


def assert_audit_contract(audit: str) -> None:
    body = code(audit)
    triggers = top_level(audit, "on")
    assert triggers is not None
    # One day of the week, at a fixed minute and hour.
    assert re.search(
        r'^  schedule:\n    - cron: "[0-9]+ [0-9]+ \* \* [0-6]"$', triggers, re.MULTILINE
    ), "the audit must run on a weekly schedule"
    assert re.search(r"^  workflow_dispatch:$", triggers, re.MULTILINE)
    paths = re.search(
        r"^  pull_request:\n    paths:\n(?P<paths>(?:      - [^\n]+\n)+)", triggers, re.MULTILINE
    )
    assert paths is not None, "the audit must run on pull requests that change dependencies"
    listed = {line.strip()[2:].strip('"') for line in paths["paths"].splitlines()}
    assert {"Cargo.lock", "Cargo.toml", "**/Cargo.toml", DOCTORS_LOCK} <= listed
    assert "workflow_call" not in triggers, "the audit must not be callable from a release"
    # One job per lockfile. As steps of one job the second scan would be
    # skipped whenever the first one fails, and so would never report.
    expected = {
        "cargo-audit": "cargo audit --deny unsound",
        "cargo-audit-doctors": f"cargo audit --deny unsound --file {DOCTORS_LOCK}",
    }
    assert (ROOT / DOCTORS_LOCK).is_file()
    blocks = jobs(audit)
    # The policy check, once per package: the workspace, and the doctors'
    # package with its own lockfile, against the same deny.toml.
    deny_checks = {
        "cargo-deny": "cargo deny --locked check",
        "cargo-deny-doctors": (
            f"cargo deny --locked --manifest-path {DOCTORS_MANIFEST} --config {DENY_CONFIG} check"
        ),
    }
    assert (ROOT / DOCTORS_MANIFEST).is_file()
    assert set(blocks) == set(expected) | set(deny_checks), (
        "one audit job per lockfile, one policy check per package, and nothing else"
    )
    deny_versions = {name: assert_deny_job(blocks[name], check) for name, check in deny_checks.items()}
    assert len(set(deny_versions.values())) == 1, "both policy checks must use the same cargo-deny"
    assert DENY_CONFIG in listed, "a change to the policy must run the policy check"
    versions = set()
    for name, block in blocks.items():
        if name not in expected:
            continue
        install = re.findall(
            r"^ +run: cargo install cargo-audit --locked --version ([0-9]+\.[0-9]+\.[0-9]+)$",
            block,
            re.MULTILINE,
        )
        assert len(install) == 1, f"{name}: cargo-audit must be installed at a pinned version"
        versions.add(install[0])
        scans = re.findall(r"^ +run: (cargo audit\b.*)$", block, re.MULTILINE)
        assert scans == [expected[name]], f"{name}: {scans}"
        # Each scan reports on its own: no job waits for, or is gated by, another.
        assert job_value(block, "needs") is None, f"{name}: must not wait for another job"
        assert job_value(block, "if") is None, f"{name}: can be skipped"
        assert not re.search(r"^\s*(?:- )?if:", block, re.MULTILINE), f"{name}: a step can be skipped"
    assert len(versions) == 1, "both scans must use the same cargo-audit"
    # No advisory is waved through, here or in cargo-audit's own configuration.
    assert "--ignore" not in body
    assert "continue-on-error" not in body
    for config in (".cargo/audit.toml", "audit.toml"):
        assert not (ROOT / config).exists(), f"{config} can hide an advisory from the audit"


def assert_deny_job(block: str, check: str) -> str:
    """The licence, source and duplicate policy in deny.toml is checked as written.

    Returns the pinned cargo-deny version the job installs.
    """
    assert (ROOT / DENY_CONFIG).is_file(), "cargo-deny has no policy to check"
    install = re.findall(
        r"^ +run: cargo install cargo-deny --locked --version ([0-9]+\.[0-9]+\.[0-9]+)$",
        block,
        re.MULTILINE,
    )
    assert len(install) == 1, "cargo-deny must be installed at a pinned version"
    # Every check, against the lockfile as committed, with no flag that
    # downgrades or leaves out a finding.
    checks = re.findall(r"^ +run: (cargo deny\b.*)$", block, re.MULTILINE)
    assert checks == [check], f"cargo-deny: {checks}"
    assert job_value(block, "needs") is None, "cargo-deny: must not wait for another job"
    assert job_value(block, "if") is None, "cargo-deny: can be skipped"
    assert not re.search(r"^\s*(?:- )?if:", block, re.MULTILINE), "cargo-deny: a step can be skipped"
    assert "continue-on-error" not in block, "cargo-deny: a finding can be ignored"
    return install[0]


# The crates that may carry MPL-2.0 (file-level copyleft), all behind
# web-fast-core's HTML parser. README.md says where their source is.
MPL_CRATES = {"cssparser", "cssparser-macros", "dtoa-short", "selectors"}
COPYLEFT = re.compile(r"(?:A|L)?GPL|SSPL|EUPL|OSL|CC-BY-SA|CDDL|EPL|MPL")


def assert_deny_policy(policy: str) -> None:
    """deny.toml refuses advisories, unknown licences and unknown sources."""
    config = tomllib.loads(policy)
    assert config["graph"] == {"all-features": True}, "optional dependencies must be checked too"

    advisories = config["advisories"]
    # No advisory is waved through, and neither a yanked nor an unmaintained
    # crate is, wherever it sits in the graph.
    assert advisories.get("ignore", []) == [], "deny.toml ignores an advisory"
    assert advisories.get("yanked") == "deny"
    assert advisories.get("unmaintained") == "all"
    assert set(advisories) <= {"ignore", "yanked", "unmaintained"}, "an advisory setting nobody reviewed"

    sources = config["sources"]
    assert sources.get("unknown-registry") == "deny"
    assert sources.get("unknown-git") == "deny"
    assert sources.get("allow-registry") == ["https://github.com/rust-lang/crates.io-index"], (
        "crates.io must be the only registry"
    )
    assert sources.get("allow-git", []) == [], "no git source is allowed"
    assert "allow-org" not in sources, "no git organisation is allowed"

    licenses = config["licenses"]
    allowed = set(licenses["allow"])
    assert {"MIT", "Apache-2.0"} <= allowed
    # No copyleft licence is allowed for every crate.
    assert not {name for name in allowed if COPYLEFT.match(name)}, "a copyleft licence is allowed globally"
    assert licenses.get("confidence-threshold", 0) >= 0.93, "a licence text may be guessed"
    # Nothing is excused from the licence check as a whole.
    assert set(licenses) <= {"allow", "confidence-threshold", "exceptions"}
    exceptions = {entry["crate"]: entry["allow"] for entry in licenses.get("exceptions", [])}
    assert len(exceptions) == len(licenses.get("exceptions", [])), "a crate is excepted twice"
    # The only exceptions are the named MPL-2.0 crates; nothing stronger.
    assert set(exceptions) == MPL_CRATES, "the MPL-2.0 exceptions changed: update README.md with them"
    assert all(allow == ["MPL-2.0"] for allow in exceptions.values())
    readme = read(ROOT / "README.md")
    for crate in MPL_CRATES:
        assert f"`{crate}`" in readme, f"README.md does not say where the source of {crate} is"

    bans = config["bans"]
    # Both exist in the tree today and only warn; neither may be switched off.
    assert bans.get("multiple-versions") in ("warn", "deny")
    assert bans.get("wildcards") in ("warn", "deny")
    assert not bans.get("skip") and not bans.get("skip-tree"), "a duplicate is hidden from the report"


def fuzz_crates() -> list[str]:
    """Every directory holding a cargo-fuzz crate."""
    return sorted(path.parent.parent.name for path in ROOT.glob("*/fuzz/Cargo.toml"))


def declared_msrv() -> str:
    version = re.search(r'^rust-version = "([0-9]+\.[0-9]+)"$', read(ROOT / "Cargo.toml"), re.MULTILINE)
    assert version is not None, "the workspace declares no rust-version"
    return version[1]


def assert_fuzz_contract(fuzz: str, ci: str | None = None, release: str | None = None) -> None:
    """fuzz.yml: scheduled, never required, every target, exact compilers."""
    ci = read(CI) if ci is None else ci
    release = read(RELEASE) if release is None else release
    body = code(fuzz)
    triggers = top_level(fuzz, "on")
    assert triggers is not None
    assert re.search(
        r'^  schedule:\n    - cron: "[0-9]+ [0-9]+ \* \* [0-6]"$', triggers, re.MULTILINE
    ), "the fuzz workflow must run on a weekly schedule"
    assert re.search(r"^  workflow_dispatch:$", triggers, re.MULTILINE)
    # Never part of a release, and on a pull request only for the files below:
    # a filtered workflow cannot be a required check.
    assert "workflow_call" not in triggers
    for caller in (ci, release):
        assert "fuzz.yml" not in code(caller), "ci.yml and release.yml must not call fuzz.yml"
    paths = re.search(
        r"^  pull_request:\n    paths:\n(?P<paths>(?:      - [^\n]+\n)+)", triggers, re.MULTILINE
    )
    assert paths is not None, "fuzz.yml must not run on every pull request"
    listed = {line.strip()[2:].strip('"') for line in paths["paths"].splitlines()}
    assert listed == {"**/fuzz/**", "tools/fuzz_package.rs", ".github/workflows/fuzz.yml"}

    blocks = jobs(fuzz)
    assert set(blocks) == {"fuzz", "miri", "msrv"}
    # No name is shared with a ci.yml job, so no required check is shadowed.
    assert not set(blocks) & set(jobs(ci))
    assert "continue-on-error" not in body, "a fuzz.yml job can fail silently"
    # Every script stops at its first failing command, and no interpreter
    # check is switched off.
    scripts = re.findall(r"^        run: \|\n(?P<body>(?:          [^\n]*\n|\n)+)", body, re.MULTILINE)
    assert scripts, "fuzz.yml has no script to check"
    for script in scripts:
        assert script.lstrip().startswith("set -euo pipefail\n"), "a fuzz.yml script can run past a failure"
    assert "MIRIFLAGS" not in body and "-Zmiri-" not in body, "a Miri check is disabled"

    # Compilers come from rustup, each named exactly through one variable.
    env = mapping(body, "env", 0)
    assert env is not None
    assert re.fullmatch(r"nightly-[0-9]{4}-[0-9]{2}-[0-9]{2}", env["NIGHTLY"]), (
        "the nightly must be named by its date"
    )
    assert env["MSRV"] == declared_msrv() + ".0", "the msrv job must build with Cargo.toml's rust-version"
    assert "dtolnay/rust-toolchain@" not in body
    installs = re.findall(r"rustup toolchain install (\S+)", body)
    assert sorted(installs) == ['"$MSRV"', '"$NIGHTLY"', '"$NIGHTLY"']
    # Every cargo command says which of them it runs with; none falls back to
    # whatever the runner's default happens to be.
    for found in re.finditer(r"(?:rustup run (?P<run>\S+) )?\bcargo (?P<first>\S+)", body):
        named = found["run"] or found["first"]
        assert named in ('"$NIGHTLY"', '"+$NIGHTLY"', '"+$MSRV"'), (
            f"`{found[0]}`: a compiler not named by a variable"
        )
    assert not re.search(r"\+(?:nightly|stable|beta)\b", body), "a floating toolchain"

    target = blocks["fuzz"]
    assert job_value(target, "if") is None, "the fuzz job must also run on its pull requests"
    crates = re.search(r"^        crate:\n(?P<body>(?:          - [^\n]+\n)+)", target, re.MULTILINE)
    assert crates is not None
    assert sorted(line.strip()[2:] for line in crates["body"].splitlines()) == fuzz_crates(), (
        "the matrix must list every crate that has a fuzz directory"
    )
    assert "      fail-fast: false\n" in target, "one crashing crate would cancel the others"
    assert "fail-fast: true" not in body
    # The list is captured, so failing to list fails the step, and counted
    # against the target files, so an empty list cannot pass.
    assert '          targets="$(cargo "+$NIGHTLY" fuzz list)"\n' in target
    assert """expected="$(find fuzz/fuzz_targets -name '*.rs' | wc -l)\"""" in target
    assert 'if [ "$listed" -eq 0 ] || [ "$listed" -ne "$expected" ]; then' in target
    assert "          for target in $targets; do\n" in target, "every target must run"
    assert "$(cargo" not in target.replace('targets="$(cargo "+$NIGHTLY" fuzz list)"', ""), (
        "a command substitution hides its exit status"
    )
    # The committed lockfile is the one fuzzed (cargo-fuzz has no --locked).
    assert (
        '          cargo "+$NIGHTLY" metadata --locked --format-version 1'
        " --manifest-path fuzz/Cargo.toml > /dev/null\n"
    ) in target
    # No libFuzzer flag turns the run into a no-op.
    flags = re.findall(r"(?<=\s)-([a-z_]+)=(\S+)", target.split(" -- \\\n", 1)[1].split("\n", 1)[0])
    assert dict(flags) == {"max_total_time": '"$FUZZ_SECONDS"', "rss_limit_mb": "2048", "timeout": "25"}, (
        f"libFuzzer flags changed: {flags}"
    )
    seconds = re.search(r"&& '([0-9]+)' \|\| '([0-9]+)'", target)
    assert seconds is not None and int(seconds[1]) >= 10 and int(seconds[2]) >= 60
    assert '"fuzz/corpus/$target" "fuzz/seeds/$target"' in target, "targets must start from the seeds"
    assert "-max_total_time=\"$FUZZ_SECONDS\"" in target and "|| failed=1" in target
    assert 'exit "$failed"' in target, "a crash must fail the job"
    assert re.search(
        r"^      FUZZ_SECONDS: \$\{\{ github\.event_name == 'pull_request' && '[0-9]+' \|\| '[0-9]+' \}\}$",
        target,
        re.MULTILINE,
    )
    upload = re.search(
        r"^      - name: [^\n]+\n        if: failure\(\)\n        uses: actions/upload-artifact@[^\n]+\n"
        r"        with:\n(?P<with>(?:          [^\n]+\n)+)",
        target,
        re.MULTILINE,
    )
    assert upload is not None, "crashing inputs must be uploaded when the job fails"
    assert "          path: ${{ matrix.crate }}/fuzz/artifacts\n" in upload["with"]
    assert len(re.findall(r"^\s*(?:- )?if:", target, re.MULTILINE)) == 1

    # The slow jobs stay off pull requests; nothing else is conditional.
    for name in ("miri", "msrv"):
        assert job_value(blocks[name], "if") == "github.event_name != 'pull_request'"
        assert len(re.findall(r"^\s*(?:- )?if:", blocks[name], re.MULTILINE)) == 1
    assert "--component miri" in blocks["miri"]
    # Each interpreted test exists, and runs natively on both instruction
    # sets: x86_64 (Linux) and aarch64 (macOS). A `--target` leg cannot build.
    assert runner_labels(blocks["miri"]) == {"ubuntu-latest", "macos-latest"}
    assert "      fail-fast: false\n" in blocks["miri"]
    interpreted = re.findall(
        r'^        run: cargo "\+\$NIGHTLY" miri test --locked -p (\S+) (--lib|--test) (\S+)$',
        blocks["miri"],
        re.MULTILINE,
    )
    assert blocks["miri"].count("miri test") == len(interpreted) == 2
    assert set(interpreted) == {
        ("xlsx-fast-core", "--lib", "vector_digit_scans"),
        ("pdf-fast-core", "--test", "obj_marker_scan"),
    }
    # A filter that matches nothing runs no test and passes.
    assert "fn vector_digit_scans_" in read(ROOT / "xlsx-fast-core/src/lib.rs")
    assert (ROOT / "pdf-fast-core/tests/obj_marker_scan.rs").is_file()
    assert '        run: cargo "+$MSRV" check --workspace --locked\n' in blocks["msrv"]


def assert_fuzz_targets_have_seeds() -> None:
    """Every fuzz target is a declared binary with at least one seed file."""
    crates = fuzz_crates()
    assert crates, "no fuzz crate found"
    for crate in crates:
        base = ROOT / crate / "fuzz"
        manifest = read(base / "Cargo.toml")
        # Outside the workspace: its own root, never a member.
        assert re.search(r"^\[workspace\]$", manifest, re.MULTILINE), f"{crate}/fuzz joins the workspace"
        assert f'"{crate}/fuzz"' not in read(ROOT / "Cargo.toml")
        declared = sorted(re.findall(r'^name = "([a-z0-9_]+)"\npath = "fuzz_targets/', manifest, re.MULTILINE))
        on_disk = sorted(path.stem for path in (base / "fuzz_targets").glob("*.rs"))
        assert declared == on_disk and declared, f"{crate}: targets declared {declared}, on disk {on_disk}"
        for target in declared:
            seeds = [path for path in (base / "seeds" / target).glob("*") if path.stat().st_size]
            assert seeds, f"{crate}: fuzz target {target} has no seed"


def assert_ci_jobs(ci: str) -> None:
    """The required checks keep their names; anything else is declared optional."""
    blocks = jobs(ci)
    assert set(blocks) == set(REQUIRED_CI_JOBS) | OPTIONAL_CI_JOBS, (
        "a ci.yml job was renamed, removed, or added without being declared"
    )
    for name, legs in REQUIRED_CI_JOBS.items():
        assert job_value(blocks[name], "name") is None, f"{name}: a display name renames the check"
        if legs is not None:
            assert runner_labels(blocks[name]) == legs, f"{name}: its matrix legs are required checks"
    assert_rustdoc_job(blocks["rustdoc"], ci)


# Features that gate code but need something CI does not have: pdf-fast-py's
# and pdf-fast-node's `reading-order` compiles only with a layout crate a
# consumer supplies.
FEATURES_NOT_BUILT_IN_CI = {"pdf-fast-py/reading-order", "pdf-fast-node/reading-order"}


def gated_features(ci: str) -> set[str]:
    """`crate/feature` for every feature that gates code in a crate's `src` or
    `tests` and that neither a default build nor `ARROW_FEATURES` turns on."""
    arrow = re.search(r"^  ARROW_FEATURES: (\S+)$", ci, re.MULTILINE)
    assert arrow is not None
    covered = set(arrow[1].split(","))
    found = set()
    for manifest in sorted(ROOT.glob("*/Cargo.toml")):
        package = tomllib.loads(read(manifest)).get("package")
        if package is None:
            continue
        features = tomllib.loads(read(manifest)).get("features", {})
        default = set(features.get("default", []))
        sources = [*manifest.parent.glob("src/**/*.rs"), *manifest.parent.glob("tests/**/*.rs")]
        used = {name for path in sources for name in re.findall(r'feature\s*=\s*"([^"]+)"', read(path))}
        for name in used & set(features) - default:
            if f"{package['name']}/{name}" not in covered:
                found.add(f"{package['name']}/{name}")
    return found


def assert_gated_features_are_built_and_tested(ci: str) -> None:
    """A feature that gates code is linted and tested in the `rust` job.

    Without this, code behind such a feature (the WMS and WFS clients, the
    memory-mapped JSON reader, the glyph catalog) compiles in nobody's
    build until a consumer turns it on.
    """
    rust = jobs(ci)["rust"]
    assert re.search(r"^  RUSTFLAGS: -D warnings$", ci, re.MULTILINE), "lints must deny warnings"
    for gated in sorted(gated_features(ci) - FEATURES_NOT_BUILT_IN_CI):
        crate, feature = gated.split("/")
        for command in (
            f"cargo clippy -p {crate} --all-targets --locked --features {feature}",
            f"cargo test -p {crate} --locked --features {feature}",
        ):
            assert f"      - run: {command}\n" in rust, f"{gated}: the rust job must run `{command}`"


def assert_rustdoc_job(docs: str, ci: str) -> None:
    """Not a required check, yet on purpose a gate: every pull request and every release.

    ci.yml carries no trigger filter and no job-level condition (other
    contracts), and release.yml calls ci.yml, so this job runs on every pull
    request and before every release. What is pinned here is that it then
    checks something: warnings denied, the pinned toolchain, the committed
    lockfile, and the feature-gated items as well as the default ones.
    """
    assert job_value(docs, "if") is None and "continue-on-error" not in docs
    assert job_value(docs, "runs-on") == "ubuntu-latest"
    assert mapping(docs, "env", 4) == {"RUSTDOCFLAGS": "-D warnings"}
    channel = re.search(r'^channel = "([^"]+)"$', read(ROOT / "rust-toolchain.toml"), re.MULTILINE)
    assert channel is not None
    assert f"          toolchain: {channel[1]}\n" in docs, "rustdoc must use the pinned toolchain"
    runs = re.findall(r"^      - run: (cargo doc\b.*)$", docs, re.MULTILINE)
    assert len(runs) == 2 and docs.count("cargo doc") == 2, f"rustdoc: {runs}"
    assert runs[0] == "cargo doc --workspace --no-deps --locked"
    prefix = "cargo doc --workspace --no-deps --locked --features "
    assert runs[1].startswith(prefix), "the second build must turn the optional features on"
    features = runs[1][len(prefix):].split(",")
    assert features[0] == "${{ env.ARROW_FEATURES }}"
    assert set(features[1:]) == {"geo-fast-core/http", "json-fast-core/mmap", "pdf-fast-core/mmap"}
    arrow = re.search(r"^  ARROW_FEATURES: (\S+)$", ci, re.MULTILINE)
    assert arrow is not None and all(name.endswith("/arrow") for name in arrow[1].split(","))


def assert_no_audit_in_the_release_path(text: str) -> None:
    body = code(text)
    for forbidden in ("cargo audit", "cargo-audit", "audit.yml", "cargo deny", "cargo-deny"):
        assert forbidden not in body, f"{forbidden}: an advisory must not block a release"


def assert_dependabot_contract(dependabot: str) -> None:
    body = code(dependabot)
    assert re.search(r"^version: 2$", body, re.MULTILINE)
    parts = re.split(r"^  - package-ecosystem: (\S+)\n", body, flags=re.MULTILINE)
    updates = {}
    for ecosystem, block in zip(parts[1::2], parts[2::2]):
        directory = re.search(r"^    directory: (\S+)$", block, re.MULTILINE)
        assert directory is not None, f"{ecosystem}: no directory"
        assert (ecosystem, directory[1]) not in updates, "the same directory is listed twice"
        updates[ecosystem, directory[1]] = block
        assert re.search(
            r"^    schedule:\n      interval: weekly$", block, re.MULTILINE
        ), f"{ecosystem}: not weekly"
    # One cargo entry per lockfile, and the actions.
    doctors = "/" + str(Path(DOCTORS_LOCK).parent)
    assert set(updates) == {("cargo", "/"), ("cargo", doctors), ("github-actions", "/")}
    grouped = re.search(
        r"^      [A-Za-z][A-Za-z0-9_|-]*:\n(?P<body>(?:        [^\n]*\n)+)",
        updates["cargo", doctors],
        re.MULTILINE,
    )
    assert grouped is not None, "the doctors' minor and patch updates must arrive together"
    assert re.search(r'^        patterns:\n          - "\*"$', grouped["body"], re.MULTILINE)
    assert "ignore:" not in updates["cargo", doctors]
    cargo = updates["cargo", "/"]
    group = re.search(
        r"^      cargo-minor-and-patch:\n(?P<body>(?:        [^\n]*\n)+)", cargo, re.MULTILINE
    )
    assert group is not None, "minor and patch updates must arrive as one pull request"
    assert re.search(r'^        patterns:\n          - "\*"$', group["body"], re.MULTILINE)
    assert re.search(
        r"^        update-types:\n          - minor\n          - patch$", group["body"], re.MULTILINE
    )
    assert "major" not in group["body"]
    # Nothing is carved out of it, by an exclusion or by a group listed before
    # it (a dependency joins the first group that matches): an excluded crate
    # would arrive on its own, and the doctors refuse Arrow crates that are not
    # all at one version.
    assert "exclude-patterns" not in group["body"]
    groups = re.findall(r"^      ([A-Za-z][A-Za-z0-9_|-]*):$", cargo, re.MULTILINE)
    assert groups[0] == "cargo-minor-and-patch"
    # A new Arrow major moves every Arrow crate in one pull request.
    arrow = re.search(
        r"^      arrow-major:\n(?P<body>(?:        [^\n]*\n)+)", cargo, re.MULTILINE
    )
    assert arrow is not None, "Arrow majors must arrive as one pull request"
    assert re.search(
        r'^        patterns:\n          - "arrow"\n          - "arrow-\*"$', arrow["body"], re.MULTILINE
    )
    assert re.search(r"^        update-types:\n          - major$", arrow["body"], re.MULTILINE)
    # pyo3 0.30 drops abi3-py38, which the wheels are built for.
    assert re.search(
        r'^      - dependency-name: "pyo3\*"\n        versions:\n          - ">=0\.30"$',
        cargo,
        re.MULTILINE,
    ), "pyo3 0.30 and later must be ignored while the wheels support Python 3.8"
    # The consumer shells load napi 2 builds, and napi-derive 3 only works with
    # the napi 3 runtime: neither major may arrive on its own.
    for crate in ("napi", "napi-derive"):
        assert re.search(
            rf'^      - dependency-name: "{re.escape(crate)}"\n        versions:\n          - ">=3"$',
            cargo,
            re.MULTILINE,
        ), f"{crate} 3 and later must be ignored while the consumer shells load napi 2 builds"
    # tiny-skia 0.12 changes rendered pixels; it waits for a reviewed change.
    assert re.search(
        r'^      - dependency-name: "tiny-skia"\n        versions:\n          - ">=0\.12"$',
        cargo,
        re.MULTILINE,
    ), "tiny-skia 0.12 and later must be ignored until its rendering change is accepted"
    # jsonwebtoken 11 accepts tokens whose header 10.3 refused; the Flight
    # servers keep 10 until that is decided.
    assert re.search(
        r'^      - dependency-name: "jsonwebtoken"\n        versions:\n          - ">=11"$',
        cargo,
        re.MULTILINE,
    ), "jsonwebtoken 11 and later must be ignored until its wider header acceptance is decided"
    # mailparse 0.18 splits MIME parts differently, and email-fast-core's
    # nesting and header-cost guards replay 0.15's search; it waits for both.
    assert re.search(
        r'^      - dependency-name: "mailparse"\n        versions:\n          - ">=0\.16"$',
        cargo,
        re.MULTILINE,
    ), "mailparse 0.16 and later must be ignored until email-fast-core's guards follow it"
    # The Arrow pin follows the consumers; every Arrow crate past it is held,
    # by one pattern, so the crates cannot drift apart.
    assert re.search(
        r'^      - dependency-name: "arrow\*"\n        versions:\n          - ">=59"$',
        cargo,
        re.MULTILINE,
    ), "Arrow 59 and later must be ignored while the consumers stay on 58"


def workflow_id(path: Path) -> str:
    return path.name


def test_the_three_workflows_exist():
    assert {path.name for path in ALL_WORKFLOWS} >= {"ci.yml", "release.yml", "audit.yml"}


@pytest.mark.parametrize("path", ALL_WORKFLOWS, ids=workflow_id)
def test_every_action_is_pinned_by_commit_sha(path: Path):
    assert_actions_pinned(read(path))
    assert_toolchain_named(read(path))


@pytest.mark.parametrize("path", ALL_WORKFLOWS, ids=workflow_id)
def test_checkouts_keep_no_credentials_and_installers_are_pinned(path: Path):
    assert_checkouts_keep_no_credentials(read(path))
    assert_installers_are_pinned(read(path))


def test_ci_triggers_carry_no_filter():
    assert_ci_runs_on_every_push_and_pull_request(read(CI))


@pytest.mark.parametrize("path", ALL_WORKFLOWS, ids=workflow_id)
def test_only_the_publishing_job_holds_a_write_token(path: Path):
    assert_least_privilege(read(path), WRITERS.get(path.name))


def test_release_grants_write_to_the_publisher_and_read_to_ci():
    assert_release_token_scopes(read(RELEASE))


@pytest.mark.parametrize("path", ALL_WORKFLOWS, ids=workflow_id)
def test_every_job_has_a_timeout(path: Path):
    assert_every_job_has_a_timeout(read(path))


def test_ci_cancels_only_superseded_pull_request_runs():
    assert_ci_concurrency(read(CI))


@pytest.mark.parametrize("path", [CI, AUDIT, FUZZ], ids=workflow_id)
def test_pull_request_workflows_use_hosted_runners_only(path: Path):
    # All run on pull requests from forks; none may reach a project machine.
    assert_hosted_runners_only(read(path))


def test_ci_needs_no_secret_and_no_token():
    assert_ci_needs_no_secret(read(CI))


def test_ci_exercises_what_a_release_builds():
    assert_ci_exercises_release_paths(read(CI))


def test_release_waits_for_every_ci_job_it_does_not_repeat():
    assert_release_waits_for_ci(read(RELEASE))


def test_ci_skips_nothing_on_a_push_or_a_pull_request():
    assert_ci_skips_only_what_the_release_rebuilds(read(CI))


def test_audit_scans_the_lockfile_and_ignores_nothing():
    assert_audit_contract(read(AUDIT))
    assert_hosted_runners_only(read(AUDIT))


@pytest.mark.parametrize("path", [CI, RELEASE], ids=workflow_id)
def test_an_advisory_cannot_block_a_release(path: Path):
    assert_no_audit_in_the_release_path(read(path))


def test_dependabot_updates_cargo_and_actions_weekly():
    assert_dependabot_contract(read(DEPENDABOT))


def test_the_dependency_policy_is_checked_and_waves_nothing_through():
    assert_deny_policy(read(ROOT / DENY_CONFIG))


def test_fuzzing_miri_and_msrv_run_weekly_and_are_never_required():
    assert_fuzz_contract(read(FUZZ))


def test_every_fuzz_target_is_declared_and_seeded():
    assert_fuzz_targets_have_seeds()


def test_the_required_checks_keep_their_names():
    assert_ci_jobs(read(CI))


def test_feature_gated_code_is_linted_and_tested():
    assert_gated_features_are_built_and_tested(read(CI))


def test_the_feature_scan_finds_the_gated_features():
    # The scan must keep finding what it guards; an empty result would make
    # the contract above pass vacuously.
    found = gated_features(read(CI))
    assert {"geo-fast-core/http", "json-fast-core/mmap", "pdf-fast-core/glyph-catalog"} <= found
    assert FEATURES_NOT_BUILT_IN_CI <= found


SHA = "3d3c42e5aac5ba805825da76410c181273ba90b1"
ELAN_URL = "https://github.com/leanprover/elan/releases/download/v4.2.4/elan-x86_64-unknown-linux-gnu.tar.gz"
ELAN_RUN = '          "$RUNNER_TEMP/elan-init" -y --no-modify-path --default-toolchain none\n'


@pytest.mark.parametrize(
    ("contract", "path", "original", "replacement"),
    [
        # A mutable tag, a short SHA, a pin without its version, a hidden step.
        (assert_actions_pinned, CI, f"actions/checkout@{SHA} # v7.0.1", "actions/checkout@v7"),
        (assert_actions_pinned, CI, f"actions/checkout@{SHA} # v7.0.1", f"actions/checkout@{SHA[:12]} # v7.0.1"),
        (assert_actions_pinned, CI, f"actions/checkout@{SHA} # v7.0.1", f"actions/checkout@{SHA}"),
        (assert_actions_pinned, CI, f"actions/checkout@{SHA} # v7.0.1", f"actions/checkout@{SHA} # v7"),
        (assert_actions_pinned, RELEASE, " # v3.0.4", " # release"),
        (assert_actions_pinned, RELEASE, "softprops/action-gh-release@", "docker://softprops/action-gh-release@"),
        (assert_actions_pinned, AUDIT, f"      - uses: actions/checkout@{SHA} # v7.0.1", "      - { uses: actions/checkout@v7 }"),
        (assert_actions_pinned, RELEASE, "uses: ./.github/workflows/ci.yml", "uses: ./.github/workflows/absent.yml"),
        (assert_actions_pinned, RELEASE, "c2c8 # master", "c2c8 # v1.98.1"),
        (assert_toolchain_named, CI, "          toolchain: 1.98.1\n          components", "          components"),
        (assert_toolchain_named, RELEASE, "          toolchain: 1.98.1\n          targets", "          targets"),
        (assert_toolchain_named, AUDIT, "toolchain: 1.98.1", "toolchain: stable"),
        # A checkout that keeps the token, an installer from a branch or unchecked, a tool at any version.
        (assert_checkouts_keep_no_credentials, CI, "          persist-credentials: false\n      - name: Install elan", "          persist-credentials: true\n      - name: Install elan"),
        (assert_checkouts_keep_no_credentials, RELEASE, "        with:\n          persist-credentials: false\n      - uses: dtolnay/rust-toolchain@683b588c487a69d01f6c6ddf40f036ff682cc2c8 # master\n        with:\n          toolchain: 1.98.1\n      - uses: actions/setup-python", "      - uses: dtolnay/rust-toolchain@683b588c487a69d01f6c6ddf40f036ff682cc2c8 # master\n        with:\n          toolchain: 1.98.1\n      - uses: actions/setup-python"),
        (assert_checkouts_keep_no_credentials, AUDIT, "          persist-credentials: false\n", "          persist-credentials: false\n          fetch-depth: 0\n"),
        (assert_installers_are_pinned, CI, "releases/download/v4.2.4/elan-x86_64", "releases/latest/download/elan-x86_64"),
        (assert_installers_are_pinned, CI, "releases/download/v4.2.4/elan-x86_64", "releases/download/v4/elan-x86_64"),
        (assert_installers_are_pinned, CI, ELAN_URL, "https://raw.githubusercontent.com/leanprover/elan/master/elan-init.sh"),
        (assert_installers_are_pinned, CI, ELAN_URL, "https://example.invalid/elan.tar.gz"),
        (assert_installers_are_pinned, CI, "            | sha256sum --check --strict\n", "            | cat\n"),
        (assert_installers_are_pinned, CI, '$RUNNER_TEMP/elan.tar.gz" \\\n            | sha256sum', '$RUNNER_TEMP/other.tar.gz" \\\n            | sha256sum'),
        (assert_installers_are_pinned, CI, '          echo "42b94d42', '          tar -tzf "$RUNNER_TEMP/elan.tar.gz"\n          echo "42b94d42'),
        (assert_installers_are_pinned, CI, ELAN_RUN, "          curl -sSf https://example.invalid/install.sh | sh -s -- -y\n"),
        (assert_installers_are_pinned, CI, ELAN_RUN, "          bash <(curl -sSf https://example.invalid/install.sh)\n"),
        (assert_installers_are_pinned, CI, ELAN_RUN, "          curl -sSfL -o /tmp/install.sh https://example.invalid/install.sh\n          sh /tmp/install.sh\n"),
        (assert_installers_are_pinned, CI, ELAN_RUN, '          sh -c "$(curl -sSf https://example.invalid/install.sh)"\n'),
        (assert_installers_are_pinned, CI, ELAN_RUN, '          eval "$(cat "$RUNNER_TEMP/elan.tar.gz")"\n'),
        (assert_installers_are_pinned, CI, ELAN_RUN, "          wget -qO- https://example.invalid/install.sh | sh\n"),
        (assert_installers_are_pinned, CI, ELAN_RUN, "          wget -q https://example.invalid/elan-init && chmod +x elan-init && ./elan-init -y\n"),
        (assert_installers_are_pinned, CI, ELAN_RUN, '          cat "$RUNNER_TEMP/elan.tar.gz" | python3\n'),
        # A trigger filter that leaves a required check unreported.
        (assert_ci_runs_on_every_push_and_pull_request, CI, "  pull_request:\n", '  pull_request:\n    paths-ignore:\n      - "**.md"\n'),
        (assert_ci_runs_on_every_push_and_pull_request, CI, "  pull_request:\n", '  pull_request:\n    paths:\n      - "**.rs"\n'),
        (assert_ci_runs_on_every_push_and_pull_request, CI, "  pull_request:\n", "  pull_request:\n    branches: [main]\n"),
        (assert_ci_runs_on_every_push_and_pull_request, CI, "  pull_request:\n", "  pull_request:\n    types: [opened]\n"),
        (assert_ci_runs_on_every_push_and_pull_request, CI, "    branches: [main]\n", "    branches: [main]\n    paths-ignore:\n      - docs/**\n"),
        (assert_ci_runs_on_every_push_and_pull_request, CI, "    branches: [main]\n", '    branches: [main]\n    paths:\n      - "**.rs"\n'),
        (assert_ci_runs_on_every_push_and_pull_request, CI, "    branches: [main]\n", "    branches-ignore: [main]\n"),
        (assert_ci_runs_on_every_push_and_pull_request, CI, "    branches: [main]\n", "    branches: [release]\n"),
        (assert_installers_are_pinned, RELEASE, "cargo install cargo-cyclonedx --locked --version 0.5.10", "cargo install cargo-cyclonedx --locked"),
        (assert_installers_are_pinned, RELEASE, "cargo install cargo-cyclonedx --locked --version 0.5.10", "cargo install cargo-cyclonedx --version 0.5.10"),
        (assert_installers_are_pinned, AUDIT, " --locked --version 0.23.0", " --locked --version 0.23"),
        # A wider default token, a second writer, a spelling that hides a grant.
        (assert_release_token_scopes, RELEASE, "permissions:\n  contents: read", "permissions:\n  contents: write"),
        (assert_release_token_scopes, RELEASE, "permissions:\n  contents: read", "permissions: write-all"),
        (assert_release_token_scopes, RELEASE, "permissions:\n  contents: read\n\nenv:", "env:"),
        (assert_release_token_scopes, RELEASE, "permissions:\n  contents: read\n", "permissions:\n  contents: read\n  packages: write\n"),
        (assert_release_token_scopes, RELEASE, "    timeout-minutes: 45\n", "    timeout-minutes: 45\n    permissions:\n      contents: write\n"),
        (assert_release_token_scopes, RELEASE, "    timeout-minutes: 45\n", "    timeout-minutes: 45\n    permissions: write-all\n"),
        (assert_release_token_scopes, RELEASE, "    permissions:\n      contents: read\n", "    permissions:\n      contents: write\n"),
        (assert_release_token_scopes, RELEASE, "    permissions:\n      contents: read\n", ""),
        (assert_release_token_scopes, RELEASE, "    permissions:\n      contents: read\n", "    permissions:\n      contents: read\n    secrets: inherit\n"),
        (assert_release_token_scopes, RELEASE, "    permissions:\n      contents: write\n", "    permissions:\n      contents: write\n      id-token: write\n"),
        (assert_release_token_scopes, RELEASE, "    runs-on: ubuntu-latest\n    timeout-minutes: 20", "    runs-on: [self-hosted, macOS]\n    timeout-minutes: 20"),
        (assert_least_privilege, CI, "permissions:\n  contents: read\n", "permissions:\n  contents: write\n"),
        (assert_least_privilege, CI, "permissions:\n  contents: read\n\n", ""),
        (assert_least_privilege, CI, "    timeout-minutes: 10\n", "    timeout-minutes: 10\n    permissions:\n      contents: write\n"),
        (assert_least_privilege, AUDIT, "  contents: read", "  contents: read\n  security-events: write"),
        # A job without a bound, with a bound that is none, or one GitHub rejects.
        (assert_every_job_has_a_timeout, CI, "    timeout-minutes: 60\n", ""),
        (assert_every_job_has_a_timeout, CI, "timeout-minutes: 10", "timeout-minutes: 360"),
        (assert_every_job_has_a_timeout, CI, "timeout-minutes: 15", "timeout-minutes: ${{ inputs.minutes }}"),
        (assert_every_job_has_a_timeout, RELEASE, "    timeout-minutes: 20\n", ""),
        (assert_every_job_has_a_timeout, RELEASE, "    uses: ./.github/workflows/ci.yml\n", "    uses: ./.github/workflows/ci.yml\n    timeout-minutes: 30\n"),
        (assert_every_job_has_a_timeout, AUDIT, "    timeout-minutes: 30\n", ""),
        # Runs on main cancelled, or pull request runs piling up.
        (assert_ci_concurrency, CI, "cancel-in-progress: ${{ github.event_name == 'pull_request' }}", "cancel-in-progress: true"),
        (assert_ci_concurrency, CI, "format('run-{0}', github.run_id)", "github.ref"),
        (assert_ci_concurrency, CI, "format('pr-{0}', github.event.pull_request.number)", "github.sha"),
        (assert_ci_concurrency, CI, "concurrency:\n", "unrelated:\n"),
        # A fork's code on a project machine, or with the base repository's token.
        (assert_hosted_runners_only, CI, "os: [macos-latest, windows-latest]", "os: [self-hosted, windows-latest]"),
        (assert_hosted_runners_only, CI, "os: [macos-latest, windows-latest]", "os: [macstudio, windows-latest]"),
        (assert_hosted_runners_only, CI, "    runs-on: ubuntu-latest\n    timeout-minutes: 10", "    runs-on: [self-hosted, macOS, ARM64, macstudio]\n    timeout-minutes: 10"),
        (assert_hosted_runners_only, CI, "  pull_request:\n", "  pull_request_target:\n"),
        (assert_hosted_runners_only, AUDIT, "runs-on: ubuntu-latest", "runs-on: macstudio"),
        (assert_ci_needs_no_secret, CI, "      - run: python3 -m pytest tests -q", "      - run: python3 -m pytest tests -q\n        env:\n          TOKEN: ${{ secrets.RELEASE_TOKEN }}"),
        (assert_ci_needs_no_secret, CI, "      - run: python3 -m pytest tests -q", "      - run: GH_TOKEN=${{ github.token }} python3 -m pytest tests -q"),
        # CI drifting away from what a release builds.
        (assert_ci_exercises_release_paths, CI, "os: [macos-latest, windows-latest]", "os: [macos-latest]"),
        (assert_ci_exercises_release_paths, CI, "os: [macos-latest, windows-latest]", "os: [windows-latest]"),
        (assert_ci_exercises_release_paths, CI, "os: [ubuntu-latest, macos-latest, windows-latest]", "os: [ubuntu-latest]"),
        (assert_ci_exercises_release_paths, CI, "        shell: bash\n    steps:", "        shell: pwsh\n    steps:"),
        (assert_ci_exercises_release_paths, CI, "    defaults:\n      run:\n        shell: bash\n    env:\n", "    env:\n"),
        (assert_ci_exercises_release_paths, CI, "xsd-fast-py\n", "\n"),
        (assert_ci_exercises_release_paths, CI, '"maturin==1.15.0"', '"maturin==1.14.0"'),
        (assert_ci_exercises_release_paths, CI, '"pyarrow==25.0.1"', '"pyarrow"'),
        (assert_ci_exercises_release_paths, CI, ' "pytest==9.1.1"', ""),
        (assert_ci_exercises_release_paths, CI, "maturin build --release --locked", "maturin build --release"),
        (assert_ci_exercises_release_paths, CI, "python -I -m pytest --import-mode=importlib -q", "python -m pytest -q"),
        (assert_ci_exercises_release_paths, CI, "python -m pip install --no-index --no-deps dist/*.whl", "python -m pip install -e ."),
        (assert_ci_exercises_release_paths, CI, "        run: node scripts/smoke-node.mjs\n", "        run: node --version\n"),
        (assert_ci_exercises_release_paths, CI, "        run: node scripts/check-node-types.mjs\n", "        run: node --version\n"),
        (assert_release_waits_for_ci, RELEASE, "        run: node scripts/check-node-types.mjs\n", "        run: node --version\n"),
        (assert_ci_exercises_release_paths, CI, "        run: scripts/build-node.sh\n", "        run: cargo build\n"),
        (assert_ci_exercises_release_paths, CI, "for manifest in *-node/package.json; do", "for manifest in *-node; do"),
        (assert_ci_exercises_release_paths, CI, "pnpm pack --pack-destination ../dist-node", "true"),
        (assert_ci_exercises_release_paths, CI, 'node-version: "24"', 'node-version: "22"'),
        (assert_ci_exercises_release_paths, CI, "          version: 10\n", "          version: 9\n"),
        # A release that no longer waits for CI, or CI that can pass while failing.
        (assert_release_waits_for_ci, RELEASE, "  wheels:\n    needs: verify\n", "  wheels:\n"),
        (assert_release_waits_for_ci, RELEASE, "  node:\n    needs: verify\n", "  node:\n    needs: []\n"),
        (assert_release_waits_for_ci, RELEASE, "    uses: ./.github/workflows/ci.yml\n", "    uses: ./.github/workflows/ci.yml\n    if: github.event_name == 'push'\n"),
        (assert_release_waits_for_ci, RELEASE, "needs: [wheels, node]", "needs: [wheels]"),
        # The release skipping more than the two jobs it rebuilds, or no longer rebuilding them.
        (assert_release_waits_for_ci, RELEASE, "    with:\n      skip_binding_jobs: true\n", ""),
        (assert_release_waits_for_ci, RELEASE, "      skip_binding_jobs: true\n", "      skip_binding_jobs: false\n"),
        (assert_release_waits_for_ci, RELEASE, "      skip_binding_jobs: true\n", "      skip_binding_jobs: true\n      skip_tests: true\n"),
        (assert_release_waits_for_ci, RELEASE, "    needs: verify\n    timeout-minutes: 45\n", "    needs: verify\n    if: github.event_name == 'push'\n    timeout-minutes: 45\n"),
        (assert_release_waits_for_ci, RELEASE, "    needs: verify\n    timeout-minutes: 60\n", "    needs: verify\n    continue-on-error: true\n    timeout-minutes: 60\n"),
        (assert_release_waits_for_ci, RELEASE, "      - name: Load and parse a generated fixture through each binding\n        run: node scripts/smoke-node.mjs\n", ""),
        (assert_release_waits_for_ci, RELEASE, 'scripts/build-node.sh "${node_bindings[@]}"', "true"),
        (assert_release_waits_for_ci, RELEASE, "for crate in $BINDINGS; do\n            (cd", "for crate in docx-fast-py; do\n            (cd"),
        # A tag released from a commit that never went through CI on main.
        (assert_release_waits_for_ci, RELEASE, "  verify:\n    needs: on-main\n", "  verify:\n"),
        (assert_release_waits_for_ci, RELEASE, "          git merge-base --is-ancestor \"$GITHUB_SHA\" origin/main\n", ""),
        (assert_release_waits_for_ci, RELEASE, "          git merge-base --is-ancestor \"$GITHUB_SHA\" origin/main\n", "          git merge-base --is-ancestor \"$GITHUB_SHA\" origin/main || true\n"),
        (assert_release_waits_for_ci, RELEASE, "--is-ancestor \"$GITHUB_SHA\" origin/main", "--is-ancestor origin/main \"$GITHUB_SHA\""),
        (assert_release_waits_for_ci, RELEASE, "            refs/tags/v*) ;;\n", "            refs/tags/v*) exit 0 ;;\n"),
        (assert_release_waits_for_ci, RELEASE, "            refs/tags/v*) ;;\n", "            refs/tags/v0*) ;;\n"),
        (assert_release_waits_for_ci, RELEASE, "      - name: Refuse a release tag whose commit is not on main\n", "      - name: Refuse a release tag whose commit is not on main\n        if: github.event_name == 'workflow_dispatch'\n"),
        (assert_release_waits_for_ci, RELEASE, "    runs-on: ubuntu-latest\n    timeout-minutes: 10\n", "    runs-on: ubuntu-latest\n    continue-on-error: true\n    timeout-minutes: 10\n"),
        (assert_release_waits_for_ci, RELEASE, "'+refs/heads/main:refs/remotes/origin/main'", "'+refs/heads/*:refs/remotes/origin/*'"),
        # A wheel uploaded from a leg where nothing installed and imported it.
        (assert_release_waits_for_ci, RELEASE, "          python -m pip install --no-index --no-deps dist/*.whl\n", ""),
        (assert_release_waits_for_ci, RELEASE, "          python -I -c \"import importlib, os; [importlib.import_module", "          python -I -c \"import importlib, os; [importlib.util.find_spec"),
        (assert_release_waits_for_ci, RELEASE, "        if: matrix.target != 'aarch64-unknown-linux-gnu'\n        shell: bash\n        run: |\n          set -euo pipefail\n          read -r -a built_bindings", "        if: matrix.target == 'x86_64-unknown-linux-gnu'\n        shell: bash\n        run: |\n          set -euo pipefail\n          read -r -a built_bindings"),
        (assert_release_waits_for_ci, RELEASE, '          test "${#built_wheels[@]}" -eq "${#built_bindings[@]}"\n', ""),
        # A CI job that can be skipped outside a release, or a third one inside it.
        (assert_ci_skips_only_what_the_release_rebuilds, CI, "    if: ${{ inputs.skip_binding_jobs != true }}\n    strategy:\n      fail-fast: false\n      matrix:\n        os: [ubuntu-latest, macos-latest, windows-latest]\n    runs-on: ${{ matrix.os }}\n    timeout-minutes: 45\n    defaults:", "    strategy:\n      fail-fast: false\n      matrix:\n        os: [ubuntu-latest, macos-latest, windows-latest]\n    runs-on: ${{ matrix.os }}\n    timeout-minutes: 45\n    if: ${{ inputs.skip_binding_jobs == true }}\n    defaults:"),
        (assert_ci_skips_only_what_the_release_rebuilds, CI, "  rust:\n    runs-on: ubuntu-latest\n", "  rust:\n    if: ${{ inputs.skip_binding_jobs != true }}\n    runs-on: ubuntu-latest\n"),
        (assert_ci_skips_only_what_the_release_rebuilds, CI, "  release-tooling:\n", "  release-tooling:\n    if: github.event_name != 'workflow_call'\n"),
        (assert_ci_skips_only_what_the_release_rebuilds, CI, "  lean-formal:\n    runs-on: ubuntu-latest\n", "  lean-formal:\n    if: false\n    runs-on: ubuntu-latest\n"),
        (assert_ci_skips_only_what_the_release_rebuilds, CI, "  lean-formal:\n    runs-on: ubuntu-latest\n", "  lean-formal:\n    continue-on-error: true\n    runs-on: ubuntu-latest\n"),
        (assert_ci_skips_only_what_the_release_rebuilds, CI, "      - run: cargo fmt --all --check\n", "      - run: cargo fmt --all --check\n        if: ${{ !inputs.skip_binding_jobs }}\n"),
        (assert_ci_skips_only_what_the_release_rebuilds, CI, "      - run: cargo test --workspace --locked\n", "      - if: github.event_name == 'pull_request'\n        run: cargo test --workspace --locked\n"),
        # A condition that skips when the input is absent, as on a push or a pull request.
        (assert_ci_skips_only_what_the_release_rebuilds, CI, "inputs.skip_binding_jobs != true }}\n    strategy:\n      fail-fast: false\n      matrix:\n        os: [ubuntu-latest, macos-latest, windows-latest]\n    runs-on: ${{ matrix.os }}\n    timeout-minutes: 45\n    # The steps", "inputs.skip_binding_jobs == false }}\n    strategy:\n      fail-fast: false\n      matrix:\n        os: [ubuntu-latest, macos-latest, windows-latest]\n    runs-on: ${{ matrix.os }}\n    timeout-minutes: 45\n    # The steps"),
        (assert_ci_skips_only_what_the_release_rebuilds, CI, "    if: ${{ inputs.skip_binding_jobs != true }}\n", "    if: ${{ inputs.skip_binding_jobs }}\n"),
        (assert_ci_skips_only_what_the_release_rebuilds, CI, "    if: ${{ inputs.skip_binding_jobs != true }}\n", "    if: ${{ github.event_name != 'workflow_call' }}\n"),
        (assert_ci_skips_only_what_the_release_rebuilds, CI, "    if: ${{ inputs.skip_binding_jobs != true }}\n", "    if: ${{ inputs.skip_binding_jobs != 'true' }}\n"),
        # An input that skips by default, is not a boolean, or can be set by hand.
        (assert_ci_skips_only_what_the_release_rebuilds, CI, "        default: false\n", "        default: true\n"),
        (assert_ci_skips_only_what_the_release_rebuilds, CI, "        default: false\n", ""),
        (assert_ci_skips_only_what_the_release_rebuilds, CI, "        type: boolean\n", "        type: string\n"),
        (assert_ci_skips_only_what_the_release_rebuilds, CI, "        required: false\n", "        required: true\n"),
        (assert_ci_skips_only_what_the_release_rebuilds, CI, "        default: false\n", "        default: false\n      skip_tests:\n        type: boolean\n        default: false\n"),
        (assert_ci_skips_only_what_the_release_rebuilds, CI, "  pull_request:\n  workflow_call:\n", "  pull_request:\n  workflow_dispatch:\n    inputs:\n      skip_binding_jobs:\n        type: boolean\n  workflow_call:\n"),
        (assert_ci_skips_only_what_the_release_rebuilds, CI, "  pull_request:\n  workflow_call:\n", "  pull_request:\n"),
        # An audit that no longer runs, no longer fails, or looks away.
        (assert_audit_contract, AUDIT, '"23 6 * * 1"', '"23 6 1 * *"'),
        (assert_audit_contract, AUDIT, "  workflow_dispatch:\n", ""),
        (assert_audit_contract, AUDIT, "      - Cargo.lock\n", ""),
        (assert_audit_contract, AUDIT, '      - "**/Cargo.toml"\n', ""),
        (assert_audit_contract, AUDIT, "  workflow_dispatch:\n", "  workflow_dispatch:\n  workflow_call:\n"),
        (assert_audit_contract, AUDIT, " --locked --version 0.23.0", " --locked"),
        (assert_audit_contract, AUDIT, "cargo install cargo-audit --locked", "cargo install cargo-audit"),
        (assert_audit_contract, AUDIT, "cargo audit --deny unsound", "cargo audit"),
        (assert_audit_contract, AUDIT, "cargo audit --deny unsound", "cargo audit --deny unsound --ignore RUSTSEC-2026-0001"),
        (assert_audit_contract, AUDIT, "cargo audit --deny unsound", "cargo audit --deny unsound || true"),
        (assert_audit_contract, AUDIT, "        run: cargo audit --deny unsound", "        continue-on-error: true\n        run: cargo audit --deny unsound"),
        (assert_audit_contract, AUDIT, "        run: cargo audit --deny unsound --file .leio-code/rust-doctors/Cargo.lock\n", "        run: cargo audit --version\n"),
        (assert_audit_contract, AUDIT, "\n  cargo-audit-doctors:\n", "\n"),
        (assert_audit_contract, AUDIT, "  cargo-audit-doctors:\n", "  cargo-audit-doctors:\n    needs: cargo-audit\n"),
        (assert_audit_contract, AUDIT, "  cargo-audit-doctors:\n", "  cargo-audit-doctors:\n    if: success()\n"),
        (assert_audit_contract, AUDIT, "      - name: Fail on a vulnerability or an unsoundness advisory in the doctors' lockfile\n", "      - name: Fail on a vulnerability or an unsoundness advisory in the doctors' lockfile\n        if: github.event_name == 'schedule'\n"),
        (assert_audit_contract, AUDIT, "    timeout-minutes: 20\n", "    timeout-minutes: 20\n    continue-on-error: true\n"),
        (assert_audit_contract, AUDIT, "cargo audit --deny unsound --file", "cargo audit --file"),
        (assert_audit_contract, AUDIT, "--file .leio-code/rust-doctors/Cargo.lock", "--file Cargo.lock"),
        (assert_audit_contract, AUDIT, "      - .leio-code/rust-doctors/Cargo.lock\n", ""),
        (assert_no_audit_in_the_release_path, CI, "      - run: cargo fmt --all --check", "      - run: cargo audit"),
        (assert_no_audit_in_the_release_path, RELEASE, "    uses: ./.github/workflows/ci.yml", "    uses: ./.github/workflows/audit.yml"),
        # The policy check dropped, unpinned, narrowed, softened or made skippable.
        (assert_audit_contract, AUDIT, "\n  cargo-deny:\n", "\n  cargo-deny-off:\n"),
        (assert_audit_contract, AUDIT, "cargo install cargo-deny --locked --version", "cargo install cargo-deny --locked #"),
        (assert_audit_contract, AUDIT, "        run: cargo deny --locked check\n", "        run: cargo deny --locked check licenses\n"),
        (assert_audit_contract, AUDIT, "        run: cargo deny --locked check\n", "        run: cargo deny check\n"),
        (assert_audit_contract, AUDIT, "        run: cargo deny --locked check\n", "        run: cargo deny --locked check || true\n"),
        (assert_audit_contract, AUDIT, "  cargo-deny:\n", "  cargo-deny:\n    if: github.event_name == 'schedule'\n"),
        (assert_audit_contract, AUDIT, "      - deny.toml\n", ""),
        # The doctors' policy check dropped, pointed at the workspace, given another
        # policy, softened, gated, made to wait, or installed at another version.
        (assert_audit_contract, AUDIT, "\n  cargo-deny-doctors:\n", "\n  cargo-deny-doctors-off:\n"),
        (assert_audit_contract, AUDIT, "--manifest-path .leio-code/rust-doctors/Cargo.toml --config", "--manifest-path Cargo.toml --config"),
        (assert_audit_contract, AUDIT, "--config deny.toml check\n", "--config doctors-deny.toml check\n"),
        (assert_audit_contract, AUDIT, " --manifest-path .leio-code/rust-doctors/Cargo.toml --config deny.toml check\n", " --manifest-path .leio-code/rust-doctors/Cargo.toml --config deny.toml check licenses\n"),
        (assert_audit_contract, AUDIT, "--config deny.toml check\n", "--config deny.toml check || true\n"),
        (assert_audit_contract, AUDIT, "  cargo-deny-doctors:\n", "  cargo-deny-doctors:\n    if: github.event_name == 'schedule'\n"),
        (assert_audit_contract, AUDIT, "  cargo-deny-doctors:\n", "  cargo-deny-doctors:\n    needs: cargo-deny\n"),
        (assert_audit_contract, AUDIT, "  cargo-deny-doctors:\n", "  cargo-deny-doctors:\n    continue-on-error: true\n"),
        (assert_audit_contract, AUDIT, "        run: cargo install cargo-deny --locked --version 0.20.2\n      - name: Check the doctors'", "        run: cargo install cargo-deny --locked --version 0.20.1\n      - name: Check the doctors'"),
        (assert_installers_are_pinned, AUDIT, "cargo install cargo-deny --locked --version", "cargo install cargo-deny --version"),
        (assert_no_audit_in_the_release_path, CI, "      - run: cargo fmt --all --check", "      - run: cargo deny check"),
        (assert_deny_policy, ROOT / DENY_CONFIG, 'unknown-registry = "deny"', 'unknown-registry = "warn"'),
        (assert_deny_policy, ROOT / DENY_CONFIG, 'unknown-git = "deny"', 'unknown-git = "allow"'),
        (assert_deny_policy, ROOT / DENY_CONFIG, "allow-git = []", 'allow-git = ["https://github.com/example/fork"]'),
        (assert_deny_policy, ROOT / DENY_CONFIG, '  "Apache-2.0",\n', '  "Apache-2.0",\n  "GPL-3.0-only",\n'),
        (assert_deny_policy, ROOT / DENY_CONFIG, '  "Apache-2.0",\n', '  "Apache-2.0",\n  "MPL-2.0",\n'),
        (assert_deny_policy, ROOT / DENY_CONFIG, "ignore = []", 'ignore = ["RUSTSEC-2026-0001"]'),
        (assert_deny_policy, ROOT / DENY_CONFIG, 'yanked = "deny"', 'yanked = "warn"'),
        (assert_deny_policy, ROOT / DENY_CONFIG, 'unmaintained = "all"', 'unmaintained = "none"'),
        (assert_deny_policy, ROOT / DENY_CONFIG, 'unmaintained = "all"\n', ""),
        (assert_deny_policy, ROOT / DENY_CONFIG, "all-features = true", "all-features = false"),
        (assert_deny_policy, ROOT / DENY_CONFIG, 'multiple-versions = "warn"', 'multiple-versions = "allow"'),
        (assert_deny_policy, ROOT / DENY_CONFIG, 'wildcards = "warn"', 'wildcards = "allow"'),
        (assert_deny_policy, ROOT / DENY_CONFIG, "confidence-threshold = 0.93", "confidence-threshold = 0.5"),
        (assert_deny_policy, ROOT / DENY_CONFIG, 'crate = "selectors"\nallow = ["MPL-2.0"]', 'crate = "selectors"\nallow = ["MPL-2.0", "GPL-3.0-only"]'),
        (assert_deny_policy, ROOT / DENY_CONFIG, 'crate = "selectors"', 'crate = "some-gpl-crate"'),
        (assert_deny_policy, ROOT / DENY_CONFIG, "\n[bans]\n", "\n[licenses.private]\nignore = true\n\n[bans]\n"),
        # Fuzzing made required or releasable, a crate left out, a floating compiler, a swallowed crash.
        (assert_fuzz_contract, FUZZ, "  pull_request:\n    paths:\n      - \"**/fuzz/**\"\n      - tools/fuzz_package.rs\n      - .github/workflows/fuzz.yml\n", "  pull_request:\n"),
        (assert_fuzz_contract, FUZZ, "  workflow_dispatch:\n", "  workflow_dispatch:\n  workflow_call:\n"),
        (assert_fuzz_contract, FUZZ, '    - cron: "41 3 * * 6"', '    - cron: "41 3 1 * *"'),
        (assert_fuzz_contract, FUZZ, "          - geo-fast-core\n", ""),
        (assert_fuzz_contract, FUZZ, "  NIGHTLY: nightly-2026-09-02", "  NIGHTLY: nightly"),
        (assert_fuzz_contract, FUZZ, "  MSRV: 1.94.0", "  MSRV: 1.98.1"),
        (assert_fuzz_contract, FUZZ, 'cargo "+$MSRV" check', "cargo +stable check"),
        (assert_fuzz_contract, FUZZ, 'rustup toolchain install "$MSRV"', "rustup toolchain install stable"),
        (assert_fuzz_contract, FUZZ, "|| failed=1", "|| true"),
        (assert_fuzz_contract, FUZZ, '          exit "$failed"\n', ""),
        (assert_fuzz_contract, FUZZ, ' "fuzz/seeds/$target"', ""),
        (assert_fuzz_contract, FUZZ, "        if: failure()\n", ""),
        (assert_fuzz_contract, FUZZ, "  msrv:\n    if: github.event_name != 'pull_request'\n", "  msrv:\n    if: false\n"),
        (assert_fuzz_contract, FUZZ, "    timeout-minutes: 60\n", "    timeout-minutes: 60\n    continue-on-error: true\n"),
        (assert_fuzz_contract, FUZZ, "  miri:\n", "  rust:\n"),
        (assert_fuzz_contract, FUZZ, "        os: [ubuntu-latest, macos-latest]", "        os: [ubuntu-latest]"),
        (assert_fuzz_contract, FUZZ, "--lib vector_digit_scans\n", "--lib vector_digit_scans --target aarch64-unknown-linux-gnu\n"),
        (assert_fuzz_contract, FUZZ, "--lib vector_digit_scans\n", "--lib no_such_test\n"),
        (assert_fuzz_contract, FUZZ, '          targets="$(cargo "+$NIGHTLY" fuzz list)"\n', '          targets="$(cargo "+$NIGHTLY" fuzz list || true)"\n'),
        (assert_fuzz_contract, FUZZ, "          for target in $targets; do\n", '          for target in $(cargo "+$NIGHTLY" fuzz list); do\n'),
        (assert_fuzz_contract, FUZZ, '          if [ "$listed" -eq 0 ] || [ "$listed" -ne "$expected" ]; then', '          if [ "$listed" -ne "$expected" ]; then'),
        (assert_fuzz_contract, FUZZ, " metadata --locked --format-version 1", " metadata --format-version 1"),
        (assert_fuzz_contract, FUZZ, "          set -euo pipefail\n          # cargo-fuzz has no", "          # cargo-fuzz has no"),
        (assert_fuzz_contract, FUZZ, " -rss_limit_mb=2048 -timeout=25", " -rss_limit_mb=2048 -timeout=25 -runs=0"),
        (assert_fuzz_contract, FUZZ, "&& '20' || '300'", "&& '20' || '0'"),
        (assert_fuzz_contract, FUZZ, "      fail-fast: false\n      matrix:\n        crate:", "      matrix:\n        crate:"),
        (assert_fuzz_contract, FUZZ, "      fail-fast: false\n      matrix:\n        os:", "      fail-fast: true\n      matrix:\n        os:"),
        (assert_fuzz_contract, FUZZ, "    timeout-minutes: 45\n    steps:\n      - uses: actions/checkout@3d3c42e5aac5ba805825da76410c181273ba90b1 # v7.0.1\n        with:\n          persist-credentials: false\n      - name: Install the dated nightly with Miri", "    timeout-minutes: 45\n    env:\n      MIRIFLAGS: -Zmiri-disable-stacked-borrows\n    steps:\n      - uses: actions/checkout@3d3c42e5aac5ba805825da76410c181273ba90b1 # v7.0.1\n        with:\n          persist-credentials: false\n      - name: Install the dated nightly with Miri"),
        (assert_fuzz_contract, FUZZ, "--test obj_marker_scan\n", "--test absent\n"),
        # The rustdoc gate left without its features, its lockfile, its pinned compiler, or made skippable.
        (assert_ci_jobs, CI, ",geo-fast-core/http,json-fast-core/mmap,pdf-fast-core/mmap", ",geo-fast-core/http"),
        (assert_ci_jobs, CI, "      - run: cargo doc --workspace --no-deps --locked\n      # Again", "      - run: cargo doc --workspace --no-deps\n      # Again"),
        (assert_ci_jobs, CI, "    env:\n      RUSTDOCFLAGS: -D warnings\n", "    if: github.event_name == 'pull_request'\n    env:\n      RUSTDOCFLAGS: -D warnings\n"),
        (assert_ci_jobs, CI, "          toolchain: 1.98.1\n      - uses: Swatinem/rust-cache@d1527c8f67ef9af357290475d4af4b0fab8f12e0 # v2.9.3\n      - run: cargo doc", "          toolchain: stable\n      - uses: Swatinem/rust-cache@d1527c8f67ef9af357290475d4af4b0fab8f12e0 # v2.9.3\n      - run: cargo doc"),
        (assert_installers_are_pinned, FUZZ, "cargo install cargo-fuzz --locked --version 0.13.2", "cargo install cargo-fuzz"),
        (assert_every_job_has_a_timeout, FUZZ, "    timeout-minutes: 60\n", ""),
        (assert_least_privilege, FUZZ, "permissions:\n  contents: read", "permissions:\n  contents: write"),
        (assert_hosted_runners_only, FUZZ, "    runs-on: ubuntu-latest\n    timeout-minutes: 60", "    runs-on: [self-hosted, linux]\n    timeout-minutes: 60"),
        # A required check renamed, removed or given a display name; an undeclared job; a silent doc build.
        (assert_ci_jobs, CI, "\n  lean-formal:\n", "\n  lean:\n"),
        (assert_ci_jobs, CI, "\n  rust:\n", "\n  rust:\n    name: Rust\n"),
        (assert_gated_features_are_built_and_tested, CI, "      - run: cargo test -p geo-fast-core --locked --features http\n", ""),
        (assert_gated_features_are_built_and_tested, CI, "      - run: cargo clippy -p geo-fast-core --all-targets --locked --features http\n", ""),
        (assert_gated_features_are_built_and_tested, CI, "      - run: cargo clippy -p json-fast-core --all-targets --locked --features mmap\n", ""),
        (assert_gated_features_are_built_and_tested, CI, "      - run: cargo test -p json-fast-core --locked --features mmap\n", ""),
        (assert_gated_features_are_built_and_tested, CI, "      - run: cargo clippy -p pdf-fast-core --all-targets --locked --features glyph-catalog\n", ""),
        (assert_gated_features_are_built_and_tested, CI, "      - run: cargo test -p pdf-fast-core --locked --features glyph-catalog\n", ""),
        (assert_gated_features_are_built_and_tested, CI, "  RUSTFLAGS: -D warnings\n", "  RUSTFLAGS: -W warnings\n"),
        (assert_ci_jobs, CI, "        os: [macos-latest, windows-latest]", "        os: [macos-latest]"),
        (assert_ci_jobs, CI, "\n  rustdoc:\n", "\n  docs:\n"),
        (assert_ci_jobs, CI, "      RUSTDOCFLAGS: -D warnings", "      RUSTDOCFLAGS: --cap-lints warn"),
        (assert_ci_jobs, CI, "      - run: cargo doc --workspace --no-deps --locked\n      # Again", "      - run: cargo doc --workspace --no-deps --locked || true\n      # Again"),
        # Dependabot left without an ecosystem, a schedule or its grouping.
        (assert_dependabot_contract, DEPENDABOT, "package-ecosystem: github-actions", "package-ecosystem: npm"),
        (assert_dependabot_contract, DEPENDABOT, "interval: weekly", "interval: monthly"),
        (assert_dependabot_contract, DEPENDABOT, "    directory: /\n    schedule:\n      interval: weekly\n    commit-message:\n      prefix: chore", "    directory: /docx-fast-core\n    schedule:\n      interval: weekly\n    commit-message:\n      prefix: chore"),
        (assert_dependabot_contract, DEPENDABOT, "    directory: /\n    schedule:\n      interval: weekly\n    commit-message:\n      prefix: ci", "    directory: /.github\n    schedule:\n      interval: weekly\n    commit-message:\n      prefix: ci"),
        (assert_dependabot_contract, DEPENDABOT, "directory: /.leio-code/rust-doctors\n", "directory: /\n"),
        (assert_dependabot_contract, DEPENDABOT, "directory: /.leio-code/rust-doctors\n", "directory: /.leio-code\n"),
        (assert_dependabot_contract, DEPENDABOT, "      doctors-minor-and-patch:\n        patterns:\n          - \"*\"\n", "      doctors-minor-and-patch:\n        patterns:\n          - \"serde*\"\n"),
        (assert_dependabot_contract, DEPENDABOT, "          - minor\n", ""),
        (assert_dependabot_contract, DEPENDABOT, "          - patch\n", "          - patch\n          - major\n"),
        (assert_dependabot_contract, DEPENDABOT, "      cargo-minor-and-patch:", "      everything:"),
        (assert_dependabot_contract, DEPENDABOT, "      arrow-major:", "      arrow-anything:"),
        (assert_dependabot_contract, DEPENDABOT, '          - "arrow-*"\n', ""),
        (assert_dependabot_contract, DEPENDABOT, '          - ">=0.30"', '          - ">=0.31"'),
        (assert_dependabot_contract, DEPENDABOT, '      - dependency-name: "napi"\n        versions:\n          - ">=3"\n', ""),
        (assert_dependabot_contract, DEPENDABOT, '"napi"\n        versions:\n          - ">=3"', '"napi"\n        versions:\n          - ">=4"'),
        (assert_dependabot_contract, DEPENDABOT, '      - dependency-name: "napi-derive"\n', '      - dependency-name: "napi-build"\n'),
        (assert_dependabot_contract, DEPENDABOT, '"napi-derive"\n        versions:\n          - ">=3"', '"napi-derive"\n        versions:\n          - ">=3.1"'),
        (assert_dependabot_contract, DEPENDABOT, '      - dependency-name: "tiny-skia"\n', '      - dependency-name: "tiny-skia-path"\n'),
        (assert_dependabot_contract, DEPENDABOT, '          - ">=0.12"', '          - ">=0.13"'),
        (assert_dependabot_contract, DEPENDABOT, '      - dependency-name: "jsonwebtoken"\n        versions:\n          - ">=11"\n', ""),
        (assert_dependabot_contract, DEPENDABOT, '"jsonwebtoken"\n        versions:\n          - ">=11"', '"jsonwebtoken"\n        versions:\n          - ">=12"'),
        (assert_dependabot_contract, DEPENDABOT, '      - dependency-name: "mailparse"\n        versions:\n          - ">=0.16"\n', ""),
        (assert_dependabot_contract, DEPENDABOT, '"mailparse"\n        versions:\n          - ">=0.16"', '"mailparse"\n        versions:\n          - ">=0.19"'),
        (assert_dependabot_contract, DEPENDABOT, '      - dependency-name: "arrow*"\n', '      - dependency-name: "arrow"\n'),
        (assert_dependabot_contract, DEPENDABOT, '          - ">=59"', '          - ">=61"'),
        (assert_dependabot_contract, DEPENDABOT, "        update-types:\n", "        exclude-patterns:\n          - arrow-*\n        update-types:\n"),
        (assert_dependabot_contract, DEPENDABOT, "      cargo-minor-and-patch:\n", "      arrow:\n        patterns:\n          - arrow-*\n      cargo-minor-and-patch:\n"),
    ],
)
def test_contracts_reject_regressions(contract, path: Path, original: str, replacement: str):
    text = read(path)
    assert original in text, f"{path.name} no longer contains {original!r}"
    contract(text)
    with pytest.raises(AssertionError):
        contract(text.replace(original, replacement))
