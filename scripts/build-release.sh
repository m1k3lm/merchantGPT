#!/usr/bin/env bash
# Builds the release binary with the publisher's Google Ads credentials
# compiled in, so merchants only have to "Sign in with Google".
# Put the values in release.env (gitignored; see release.env.example) or
# export them in the environment, e.g. from CI secrets. crates/core/build.rs
# reads release.env on any cargo build; this script adds the missing-value
# warnings and picks up a release.env created since the last build.
#   bash scripts/build-release.sh
set -euo pipefail
cd "$(dirname "$0")/.."
if [ -f release.env ]; then
  set -a
  # shellcheck disable=SC1091
  . ./release.env
  set +a
fi
for v in MERCHANTGPT_GOOGLE_OAUTH_CLIENT_ID MERCHANTGPT_GOOGLE_OAUTH_CLIENT_SECRET MERCHANTGPT_GOOGLE_ADS_DEVELOPER_TOKEN; do
  if [ -z "${!v:-}" ]; then
    echo "warning: $v is not set; users of this build will be asked for it" >&2
  fi
done
cargo build --release "$@"
