#!/usr/bin/env bash
# Full quality gate. Run inside the dev container:
#   docker compose run --rm dev bash scripts/check.sh
set -euo pipefail
cargo build --all-targets
cargo test 2>&1 | grep -E "^test result|FAILED|panicked|^error"
cargo clippy --all-targets -- -D warnings
cargo fmt --check
echo "ALL_CHECKS_PASSED"
