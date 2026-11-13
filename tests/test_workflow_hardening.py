"""Supply-chain and least-privilege invariants of the GitHub Actions workflows.

The workflows are read as text, like the other workflow tests: the suite runs
with nothing but pytest installed, and a version comment after a pinned action
is invisible to a YAML parser anyway. Every helper below therefore relies on the
block style these files are written in (two-space indentation, one key per
line), and each contract refuses a spelling it cannot read.
"""

from __future__ import annotations

import re
from pathlib import Path

import pytest

ROOT = Path(__file__).resolve().parents[1]
WORKFLOWS = ROOT / ".github/workflows"
CI = WORKFLOWS / "ci.yml"
RELEASE = WORKFLOWS / "release.yml"
AUDIT = WORKFLOWS / "audit.yml"
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


def assert_installers_are_pinned(text: str) -> None:
    """Nothing is piped from the network into a shell or installed unversioned."""
    body = code(text)
    assert not re.search(r"\|\s*(?:ba)?sh\b", body), "a download is piped into a shell"
    for url in re.findall(r"https://raw\.githubusercontent\.com/\S+", body):
        assert re.fullmatch(
            r"https://raw\.githubusercontent\.com/[\w.-]+/[\w.-]+/[0-9a-f]{40}/\S+", url
        ), f"{url}: not fetched at a full commit SHA"
        name = url.rsplit("/", 1)[1]
        assert re.search(
            rf'echo "[0-9a-f]{{64}}  \$RUNNER_TEMP/{re.escape(name)}" \\\n\s+\| sha256sum --check --strict\n\s+sh "\$RUNNER_TEMP/{re.escape(name)}"',
            body,
        ), f"{name}: must be checked against a SHA-256 before it runs"
    for install in re.findall(r"cargo install [^\n]+", body):
        assert re.search(r" --locked\b", install) and re.search(
            r" --version [0-9]+\.[0-9]+\.[0-9]+(?:\s|$)", install
        ), f"`{install}`: not pinned to an exact version"


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


def assert_release_waits_for_ci(release: str, ci: str | None = None) -> None:
    ci = read(CI) if ci is None else ci
    blocks = required_jobs(release, "verify", "wheels", "node", "manifest")
    verify = blocks["verify"]
    assert job_value(verify, "uses") == "./.github/workflows/ci.yml"
    assert job_value(verify, "if") is None, "the CI gate must never be skipped"
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
    assert set(blocks) == set(expected), "one audit job per lockfile, and nothing else"
    versions = set()
    for name, block in blocks.items():
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


def assert_no_audit_in_the_release_path(text: str) -> None:
    body = code(text)
    for forbidden in ("cargo audit", "cargo-audit", "audit.yml"):
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


@pytest.mark.parametrize("path", [CI, AUDIT], ids=workflow_id)
def test_pull_request_workflows_use_hosted_runners_only(path: Path):
    # Both run on pull requests from forks; neither may reach a project machine.
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


SHA = "3d3c42e5aac5ba805825da76410c181273ba90b1"


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
        (assert_installers_are_pinned, CI, "elan/55dc2814ca42ace25f05ba4dd7a43a8c701a0549/elan-init.sh", "elan/master/elan-init.sh"),
        (assert_installers_are_pinned, CI, "            | sha256sum --check --strict\n", "            | cat\n"),
        (assert_installers_are_pinned, CI, '          sh "$RUNNER_TEMP/elan-init.sh" -y', '          curl -sSf https://example.invalid/install.sh | sh -s -- -y'),
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
