#!/usr/bin/env bash
set -euo pipefail

protocol_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
workspace_root="$(cd "${protocol_root}/../.." && pwd)"
temporary_root="$(mktemp -d)"
trap 'rm -rf "${temporary_root}"' EXIT

clone_root="${temporary_root}/cookie-agent"
mkdir "${clone_root}"
tar --exclude=.git --exclude=target --exclude=--check -C "${workspace_root}" -cf - . | tar -C "${clone_root}" -xf -

if (
  cd "${clone_root}"
  cargo run --locked -p cookie_agent_protocol --example generate -- --check
); then
  echo "binding generator unexpectedly accepted --check as an output directory" >&2
  exit 1
fi
test ! -e "${clone_root}/--check"

(
  cd "${clone_root}"
  ./crates/protocol/scripts/check-bindings.sh --check
)

test ! -e "${clone_root}/--check"
test ! -e "${temporary_root}/--check"
