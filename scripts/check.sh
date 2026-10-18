#!/usr/bin/env bash
# Full quality gate. Run inside the dev container:
#   docker compose run --rm dev bash scripts/check.sh
set -euo pipefail
# Linking every test binary at once runs the Docker VM out of memory
# (`ld terminated with signal 9`); CARGO_BUILD_JOBS overrides.
export CARGO_BUILD_JOBS="${CARGO_BUILD_JOBS:-4}"
cargo build --all-targets
cargo test 2>&1 | grep -E "^test result|FAILED|panicked|^error"
cargo clippy --all-targets -- -D warnings
cargo fmt --check
echo "ALL_CHECKS_PASSED"
