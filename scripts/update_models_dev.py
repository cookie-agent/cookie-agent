#!/usr/bin/env python3
"""Developer-only offline checker and explicit bootstrap updater.

This script is never invoked by Cargo build scripts, runtime code, or tests.
It intentionally uses the authoritative upstream Bun generator at the recorded
bootstrap provenance commit rather than the repository-root models.json file. ``--check`` is
strictly offline and never clones or installs dependencies. Only ``--update``
may clone or run ``bun install``.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import pathlib
import re
import shutil
import subprocess
import tempfile


REPOSITORY = "https://github.com/anomalyco/models.dev.git"
BOOTSTRAP_COMMIT = "c3057690bbb8bd41cafdefadcd2a7b958e2a4642"
EXPECTED_BOOTSTRAP_SIZE = 3_567_054
EXPECTED_BOOTSTRAP_SHA256 = "d65af0b058204954f6b08af537fa13e91f251c618d69d8c20a2d5915731d482a"
ROOT = pathlib.Path(__file__).resolve().parents[1]
BOOTSTRAP_OUTPUT = ROOT / "crates/models/catalog/models-dev.json"
LICENSE_OUTPUT = ROOT / "crates/models/catalog/LICENSE.models.dev"
PROVENANCE_OUTPUT = ROOT / "crates/models/catalog/README.md"
INTEGRITY_CONSTANTS = ROOT / "crates/models/src/catalog/bootstrap.rs"


def run(*args: str, cwd: pathlib.Path) -> str:
    result = subprocess.run(
        args,
        cwd=cwd,
        check=True,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
    )
    return result.stdout.strip()


def verify_checkout(source: pathlib.Path) -> None:
    found = run("git", "rev-parse", "HEAD", cwd=source)
    if found != BOOTSTRAP_COMMIT:
        raise SystemExit(
            f"models.dev checkout is {found}; expected bootstrap provenance {BOOTSTRAP_COMMIT}"
        )


def dependencies_present(source: pathlib.Path) -> bool:
    return all(
        (source / path).is_file()
        for path in [
            "node_modules/zod/package.json",
            "node_modules/remeda/package.json",
            "node_modules/@models.dev/core/package.json",
        ]
    )


def ensure_source_dependencies(
    source: pathlib.Path,
    *,
    allow_install: bool,
    runner=run,
    bun_path: str | None = None,
) -> None:
    if dependencies_present(source):
        return
    if not allow_install:
        raise SystemExit(
            "offline check requires dependencies already present in the pinned "
            "models.dev checkout; run `bun install --frozen-lockfile` there "
            "before checking, or use explicit `--update` to allow installation"
        )
    bun = bun_path or shutil.which("bun")
    if bun is None:
        raise SystemExit("bun is required for explicit --update dependency installation")
    runner(bun, "install", "--frozen-lockfile", cwd=source)
    if not dependencies_present(source):
        raise SystemExit("bun install completed without the required pinned dependencies")


def generate(
    source: pathlib.Path,
    destination: pathlib.Path,
    *,
    allow_install: bool,
) -> bytes:
    ensure_source_dependencies(source, allow_install=allow_install)
    bun = shutil.which("bun")
    if bun is None:
        raise SystemExit("bun is required to run the authoritative models.dev generator")
    expression = (
        'import { loadCatalog, snapshotPayload } from '
        '"./packages/sdk/script/generate.ts"; '
        f'await Bun.write({str(destination)!r}, snapshotPayload(await loadCatalog()))'
    )
    run(bun, "--no-install", "-e", expression, cwd=source)
    return destination.read_bytes()


def verify_payload(payload: bytes) -> None:
    digest = hashlib.sha256(payload).hexdigest()
    if len(payload) != EXPECTED_BOOTSTRAP_SIZE:
        raise SystemExit(
            f"generated size is {len(payload)}; expected {EXPECTED_BOOTSTRAP_SIZE}"
        )
    if digest != EXPECTED_BOOTSTRAP_SHA256:
        raise SystemExit(
            f"generated SHA256 is {digest}; expected {EXPECTED_BOOTSTRAP_SHA256}"
        )
    if payload.endswith(b"\n"):
        raise SystemExit("generated payload unexpectedly has a trailing newline")
    verify_reasoning_options(payload)


def source_license(source: pathlib.Path) -> bytes:
    path = source / "LICENSE"
    if not path.is_file():
        raise SystemExit("models.dev checkout has no LICENSE provenance file")
    payload = path.read_bytes()
    if b"MIT License" not in payload or b"Copyright" not in payload:
        raise SystemExit("models.dev LICENSE is not the expected MIT provenance")
    return payload


def verify_recorded_provenance(license_payload: bytes) -> None:
    if not LICENSE_OUTPUT.is_file() or LICENSE_OUTPUT.read_bytes() != license_payload:
        raise SystemExit(f"{LICENSE_OUTPUT} does not match the bootstrap source license")
    constants = INTEGRITY_CONSTANTS.read_text(encoding="utf-8")
    for required in [
        BOOTSTRAP_COMMIT,
        f"pub const MODELS_DEV_BOOTSTRAP_BYTES: usize = {EXPECTED_BOOTSTRAP_SIZE:_};",
        EXPECTED_BOOTSTRAP_SHA256,
    ]:
        if required not in constants:
            raise SystemExit(f"{INTEGRITY_CONSTANTS} is missing bootstrap integrity provenance")
    provenance = PROVENANCE_OUTPUT.read_text(encoding="utf-8")
    if (
        f"{EXPECTED_BOOTSTRAP_SIZE:,} bytes" not in provenance
        or f"sha256:{EXPECTED_BOOTSTRAP_SHA256}" not in provenance
    ):
        raise SystemExit(f"{PROVENANCE_OUTPUT} is missing bootstrap provenance")


def update_recorded_provenance(license_payload: bytes) -> None:
    LICENSE_OUTPUT.write_bytes(license_payload)
    constants = INTEGRITY_CONSTANTS.read_text(encoding="utf-8")
    constants = re.sub(
        r'pub const MODELS_DEV_BOOTSTRAP_COMMIT: &str = "[0-9a-f]{40}";',
        f'pub const MODELS_DEV_BOOTSTRAP_COMMIT: &str = "{BOOTSTRAP_COMMIT}";',
        constants,
    )
    constants = re.sub(
        r"pub const MODELS_DEV_BOOTSTRAP_BYTES: usize = [0-9_]+;",
        f"pub const MODELS_DEV_BOOTSTRAP_BYTES: usize = {EXPECTED_BOOTSTRAP_SIZE:_};",
        constants,
    )
    constants = re.sub(
        r'(?s)(pub const MODELS_DEV_BOOTSTRAP_SHA256: &str =\s*)"[0-9a-f]{64}";',
        rf'\g<1>"{EXPECTED_BOOTSTRAP_SHA256}";',
        constants,
    )
    INTEGRITY_CONSTANTS.write_text(constants, encoding="utf-8")
    provenance = PROVENANCE_OUTPUT.read_text(encoding="utf-8")
    provenance = re.sub(
        r"Bundled artifact facts are [0-9,]+ bytes and\n`sha256:[0-9a-f]{64}`\.",
        f"Bundled artifact facts are {EXPECTED_BOOTSTRAP_SIZE:,} bytes and\n"
        f"`sha256:{EXPECTED_BOOTSTRAP_SHA256}`.",
        provenance,
    )
    PROVENANCE_OUTPUT.write_text(provenance, encoding="utf-8")
    verify_recorded_provenance(license_payload)


def verify_reasoning_options(payload: bytes) -> None:
    catalog = json.loads(payload)
    efforts = {"none", "minimal", "low", "medium", "high", "xhigh", "max", "default"}
    for provider_id, provider in catalog["providers"].items():
        for model_id, model in provider["models"].items():
            for option in model.get("reasoning_options", []):
                option_type = option.get("type")
                path = f"{provider_id}/{model_id}.reasoning_options"
                if option_type == "effort":
                    if set(option) != {"type", "values"} or not isinstance(option["values"], list):
                        raise SystemExit(f"invalid effort option at {path}")
                    normalized = []
                    for value in option["values"]:
                        if value is not None and value not in efforts:
                            raise SystemExit(f"invalid effort value at {path}")
                        normalized.append(value)
                    if len(normalized) != len({json.dumps(value) for value in normalized}):
                        raise SystemExit(f"duplicate effort value at {path}")
                elif option_type == "toggle":
                    if set(option) != {"type"}:
                        raise SystemExit(f"invalid toggle option at {path}")
                elif option_type == "budget_tokens":
                    if not set(option).issubset({"type", "min", "max"}):
                        raise SystemExit(f"invalid budget_tokens option at {path}")
                    minimum = option.get("min")
                    maximum = option.get("max")
                    if minimum is not None and (isinstance(minimum, bool) or not isinstance(minimum, int) or minimum < -1):
                        raise SystemExit(f"invalid budget_tokens min at {path}")
                    if maximum is not None and (isinstance(maximum, bool) or not isinstance(maximum, int) or maximum < 0):
                        raise SystemExit(f"invalid budget_tokens max at {path}")
                    if minimum is not None and maximum is not None and minimum >= 0 and minimum > maximum:
                        raise SystemExit(f"inverted budget_tokens bounds at {path}")
                else:
                    raise SystemExit(f"unknown reasoning option at {path}")


def self_test() -> None:
    with tempfile.TemporaryDirectory(prefix="cookie-agent-models-dev-test-") as temporary:
        source = pathlib.Path(temporary)
        commands: list[tuple[str, ...]] = []

        def fake_run(*args: str, cwd: pathlib.Path) -> str:
            commands.append(args)
            (cwd / "node_modules/zod").mkdir(parents=True, exist_ok=True)
            (cwd / "node_modules/remeda").mkdir(parents=True, exist_ok=True)
            (cwd / "node_modules/@models.dev/core").mkdir(parents=True, exist_ok=True)
            (cwd / "node_modules/zod/package.json").write_text("{}", encoding="utf-8")
            (cwd / "node_modules/remeda/package.json").write_text("{}", encoding="utf-8")
            (cwd / "node_modules/@models.dev/core/package.json").write_text(
                "{}", encoding="utf-8"
            )
            return ""

        try:
            ensure_source_dependencies(source, allow_install=False, runner=fake_run)
        except SystemExit as error:
            assert "offline check requires dependencies already present" in str(error)
        else:
            raise AssertionError("offline dependency check unexpectedly succeeded")
        assert commands == [], "offline dependency check executed a command"

        ensure_source_dependencies(
            source,
            allow_install=True,
            runner=fake_run,
            bun_path="bun",
        )
        assert commands == [("bun", "install", "--frozen-lockfile")]
        verify_reasoning_options(
            json.dumps(
                {
                    "providers": {
                        "test": {
                            "models": {
                                "model": {
                                    "reasoning_options": [
                                        {"type": "effort", "values": ["low", None]},
                                        {"type": "toggle"},
                                        {"type": "budget_tokens", "min": -1, "max": 1024},
                                    ]
                                }
                            }
                        }
                    }
                }
            ).encode()
        )
        try:
            verify_reasoning_options(
                b'{"providers":{"test":{"models":{"model":{"reasoning_options":[{"type":"budget_tokens","min":2,"max":1}]}}}}}'
            )
        except SystemExit as error:
            assert "inverted budget_tokens bounds" in str(error)
        else:
            raise AssertionError("invalid reasoning metadata unexpectedly passed")
        print("self-test ok: offline mode executes no install/network command")


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument(
        "--source",
        type=pathlib.Path,
        help="existing exact pinned models.dev checkout",
    )
    mode = parser.add_mutually_exclusive_group(required=True)
    mode.add_argument(
        "--check",
        action="store_true",
        help="strictly offline verification; requires --source and installed dependencies",
    )
    mode.add_argument(
        "--update",
        action="store_true",
        help="explicitly allow clone/dependency installation and replace the artifact",
    )
    mode.add_argument("--self-test", action="store_true", help=argparse.SUPPRESS)
    args = parser.parse_args()

    if args.self_test:
        self_test()
        return
    if args.check and args.source is None:
        parser.error("--check requires --source; offline mode never clones")

    with tempfile.TemporaryDirectory(prefix="cookie-agent-models-dev-") as temporary:
        temporary_path = pathlib.Path(temporary)
        if args.source is None:
            assert args.update
            source = temporary_path / "models.dev"
            subprocess.run(
                ["git", "clone", "--quiet", REPOSITORY, str(source)], check=True
            )
            run("git", "checkout", "--quiet", BOOTSTRAP_COMMIT, cwd=source)
        else:
            source = args.source.resolve()
        verify_checkout(source)
        license_payload = source_license(source)
        generated_path = temporary_path / "models-dev.json"
        payload = generate(source, generated_path, allow_install=args.update)
        verify_payload(payload)

        if args.check:
            if not BOOTSTRAP_OUTPUT.is_file() or BOOTSTRAP_OUTPUT.read_bytes() != payload:
                raise SystemExit(f"{BOOTSTRAP_OUTPUT} is not the recorded bootstrap payload")
            verify_recorded_provenance(license_payload)
            print(
                f"ok bootstrap: {EXPECTED_BOOTSTRAP_SHA256} "
                f"({EXPECTED_BOOTSTRAP_SIZE} bytes)"
            )
            return

        assert args.update
        BOOTSTRAP_OUTPUT.parent.mkdir(parents=True, exist_ok=True)
        BOOTSTRAP_OUTPUT.write_bytes(payload)
        update_recorded_provenance(license_payload)
        print(
            f"updated bootstrap {BOOTSTRAP_OUTPUT}: {EXPECTED_BOOTSTRAP_SHA256} "
            f"({EXPECTED_BOOTSTRAP_SIZE} bytes); runtime network selection remains unpinned"
        )


if __name__ == "__main__":
    main()
