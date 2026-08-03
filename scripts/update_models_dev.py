#!/usr/bin/env python3
"""Developer-only offline checker and explicit opt-in catalog updater.

This script is never invoked by Cargo build scripts, runtime code, or tests.
It intentionally uses the authoritative upstream Bun generator at the pinned
commit rather than the repository-root models.json file. ``--check`` is
strictly offline and never clones or installs dependencies. Only ``--update``
may clone or run ``bun install``.
"""

from __future__ import annotations

import argparse
import hashlib
import pathlib
import shutil
import subprocess
import tempfile


REPOSITORY = "https://github.com/anomalyco/models.dev.git"
COMMIT = "c3057690bbb8bd41cafdefadcd2a7b958e2a4642"
EXPECTED_SIZE = 3_567_054
EXPECTED_SHA256 = "d65af0b058204954f6b08af537fa13e91f251c618d69d8c20a2d5915731d482a"
ROOT = pathlib.Path(__file__).resolve().parents[1]
OUTPUT = ROOT / "crates/models/catalog/models-dev.json"


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
    if found != COMMIT:
        raise SystemExit(f"models.dev checkout is {found}; expected {COMMIT}")


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
    if len(payload) != EXPECTED_SIZE:
        raise SystemExit(f"generated size is {len(payload)}; expected {EXPECTED_SIZE}")
    if digest != EXPECTED_SHA256:
        raise SystemExit(f"generated SHA256 is {digest}; expected {EXPECTED_SHA256}")
    if payload.endswith(b"\n"):
        raise SystemExit("generated payload unexpectedly has a trailing newline")


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
            run("git", "checkout", "--quiet", COMMIT, cwd=source)
        else:
            source = args.source.resolve()
        verify_checkout(source)
        generated_path = temporary_path / "models-dev.json"
        payload = generate(source, generated_path, allow_install=args.update)
        verify_payload(payload)

        if args.check:
            if not OUTPUT.is_file() or OUTPUT.read_bytes() != payload:
                raise SystemExit(f"{OUTPUT} is not the pinned canonical payload")
            print(f"ok: {EXPECTED_SHA256} ({EXPECTED_SIZE} bytes)")
            return

        assert args.update
        OUTPUT.parent.mkdir(parents=True, exist_ok=True)
        OUTPUT.write_bytes(payload)
        print(f"updated {OUTPUT}: {EXPECTED_SHA256} ({EXPECTED_SIZE} bytes)")


if __name__ == "__main__":
    main()
