#!/usr/bin/env bash
# Runs the local gate tier (AGENTS.md, "Gate tiers") for what changed since
# the merge base with a base revision, uncommitted and untracked files
# included:
#
#   - formatting, always;
#   - Clippy with warnings denied and the tests of every changed crate and
#     every workspace crate that depends on it;
#   - Clippy for wasm32 when the protocol, client state, or web crates
#     changed, and the protocol client's browser tests when the protocol did;
#   - the protocol compatibility check when crates/protocol changed;
#   - the web bundle (the quick --fast one) and the browser scenarios when
#     the web client or its scenarios changed;
#   - the documentation build when docs/ or mkdocs.yml changed, without
#     rustdoc unless a crate changed too.
#
# The tests make their files under TMPDIR, which on Linux is a directory of
# its own in /dev/shm when that is a tmpfs with room to spare: their many
# small writes and fsyncs then never wait on the disk. A TMPDIR already set
# is kept.
#
# A change outside these paths that may affect every crate (Cargo.toml,
# Cargo.lock, test-support/, examples/, ...) checks the whole workspace.
#
# Usage: scripts/check-changed.sh [-n|--dry-run] [base]   (base: origin/main)
set -euo pipefail

dry_run=false
if [[ "${1:-}" == "-n" || "${1:-}" == "--dry-run" ]]; then
    dry_run=true
    shift
fi
base="${1:-origin/main}"

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$root"

merge_base="$(git merge-base "$base" HEAD)"
mapfile -t changed < <(
    {
        git diff --name-only "$merge_base"
        git ls-files --others --exclude-standard
    } | sort -u
)
if ((${#changed[@]} == 0)); then
    echo "nothing changed since $base"
    exit 0
fi

# Crate directories (crates/<dir>) whose crates, and their dependents, get
# Clippy and tests; crates whose own tests check a changed file; and the
# other checks to run.
crate_dirs=()
test_only=()
workspace=false wasm=false wasm_client=false schema=false web=false docs=false workflows=false
for path in "${changed[@]}"; do
    case "$path" in
        crates/protocol/*) crate_dirs+=(protocol) wasm=true wasm_client=true schema=true ;;
        crates/client_state/*) crate_dirs+=(client_state) wasm=true ;;
        crates/web/*) crate_dirs+=(web) wasm=true web=true ;;
        crates/web_assets/*) crate_dirs+=(web_assets) web=true ;;
        crates/*/*)
            dir="${path#crates/}"
            crate_dirs+=("${dir%%/*}")
            ;;
        # Pages that tests keep in step with the code.
        docs/site/clients/cli.md | docs/site/reference/tools.md)
            test_only+=(cookie_agent) docs=true ;;
        docs/site/reference/protocol.md | docs/site/reference/errors.md)
            test_only+=(cookie_agent_protocol) docs=true ;;
        docs/* | mkdocs.yml | requirements-docs.txt | scripts/build-docs.sh | scripts/rustdoc-index.html)
            docs=true ;;
        # release_integrity pins CI's gates.
        .github/workflows/ci.yml) test_only+=(cookie_agent_models) workflows=true ;;
        .github/workflows/*) workflows=true ;;
        scripts/web-e2e/chrome.sh) web=true wasm_client=true ;;
        scripts/web-e2e/* | scripts/test-web-page.sh | scripts/build-web.sh) web=true ;;
        scripts/test-wasm-client.sh) wasm_client=true ;;
        scripts/update_models_dev.py) crate_dirs+=(models) ;;
        *.md | .github/* | scripts/*) ;;
        *) workspace=true ;;
    esac
done

# The packages to check: the changed crates and, transitively, every
# workspace crate that depends on one (dev-dependencies too, whose tests
# use them), from `cargo metadata`.
packages=()
if $workspace || ((${#crate_dirs[@]})); then
    mapfile -t packages < <(
        cargo metadata --locked --no-deps --format-version 1 |
            python3 -I -c '
import json, os, sys
workspace, dirs = sys.argv[1] == "true", set(sys.argv[2:])
packages = json.load(sys.stdin)["packages"]
by_dir = {os.path.basename(os.path.dirname(p["manifest_path"])): p["name"] for p in packages}
dependents = {p["name"]: set() for p in packages}
for p in packages:
    for dependency in p["dependencies"]:
        if dependency.get("path") and dependency["name"] in dependents:
            dependents[dependency["name"]].add(p["name"])
chosen = set(dependents) if workspace else {by_dir[d] for d in dirs if d in by_dir}
pending = list(chosen)
while pending:
    for dependent in dependents[pending.pop()] - chosen:
        chosen.add(dependent)
        pending.append(dependent)
print("\n".join(sorted(chosen)))
' "$workspace" "${crate_dirs[@]}"
    )
fi
# Crates checked in full need no separate run of their own tests.
extra_tests=()
for crate in $(printf '%s\n' "${test_only[@]}" | sort -u); do
    [[ " ${packages[*]} " == *" $crate "* ]] || extra_tests+=("$crate")
done

# A tmpfs TMPDIR for the tests, removed when the script exits.
test_env=
if [[ -z "${TMPDIR:-}" && "$(uname -s)" == Linux ]] &&
    [[ "$(stat -f -c %T /dev/shm 2>/dev/null)" == tmpfs ]] &&
    ! findmnt -n -o OPTIONS /dev/shm 2>/dev/null | grep -qw noexec &&
    (($(df --output=avail -B1G /dev/shm 2>/dev/null | tail -1) >= 8)); then
    test_tmp="$(mktemp -d /dev/shm/cookie-agent-tests.XXXXXX)"
    trap 'rm -rf "$test_tmp"' EXIT
    test_env="TMPDIR=$test_tmp "
fi

steps=()
step() { steps+=("$*"); }
# Tests under cargo-nextest when it is installed, as CI runs them, with the
# doctests, which nextest does not run, under cargo test.
tests() {
    if cargo nextest --version >/dev/null 2>&1; then
        step "${test_env}cargo nextest run --locked --no-fail-fast $1"
        step "${test_env}cargo test --doc --locked $1"
    else
        step "${test_env}cargo test --locked --no-fail-fast $1"
    fi
}
step cargo fmt --all -- --check
if ((${#packages[@]})); then
    selection="$(printf -- '-p %s ' "${packages[@]}")"
    step cargo clippy --locked "$selection"--all-targets -- -D warnings
    tests "$selection"
    # CI builds rustdoc with warnings denied on every push, so a doc link
    # to a private item fails there; catch it here for the changed crates.
    step "RUSTDOCFLAGS='-D warnings' cargo doc --locked --no-deps $selection"
fi
if ((${#extra_tests[@]})); then
    tests "$(printf -- '-p %s ' "${extra_tests[@]}")"
fi
if $wasm; then
    step cargo clippy --locked --target wasm32-unknown-unknown -p cookie_agent_protocol --features websocket \
        -p cookie_agent_client_state -p cookie_agent_web --all-targets -- -D warnings
fi
$schema && step crates/protocol/scripts/check-schema-additive.sh
$wasm_client && step scripts/test-wasm-client.sh
if $web; then
    step scripts/build-web.sh --fast
    step scripts/test-web-page.sh
fi
# Rustdoc only when a crate changed too: CI builds it with warnings denied.
if $docs; then
    if $workspace || ((${#crate_dirs[@]})); then
        step ./scripts/build-docs.sh
    else
        step ./scripts/build-docs.sh --no-rustdoc
    fi
fi
if $workflows && command -v actionlint >/dev/null; then
    # docs.yml's `concurrency.queue` is newer GitHub syntax than actionlint
    # 1.7 knows; the ignore drops only that report.
    step "actionlint -ignore 'unexpected key \"queue\" for \"concurrency\" section'"
fi

echo "changed since $base ($(git rev-parse --short "$merge_base")): ${#changed[@]} files"
for command in "${steps[@]}"; do
    echo "+ $command"
    $dry_run || eval "$command"
done
