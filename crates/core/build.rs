//! Compiles the publisher's Google Ads credentials into the app from the
//! gitignored `release.env` at the workspace root, so any `cargo build` picks
//! them up. A variable already set in the environment (CI secrets) wins.
//!
//! The file is watched only while it exists: watching a missing path makes
//! cargo rebuild this crate on every build. So a `release.env` created after
//! the last build is first noticed by `scripts/build-release.sh`, whose
//! exported variables force a rerun.

use std::path::Path;

const VARS: [&str; 3] = [
    "MERCHANTGPT_GOOGLE_OAUTH_CLIENT_ID",
    "MERCHANTGPT_GOOGLE_OAUTH_CLIENT_SECRET",
    "MERCHANTGPT_GOOGLE_ADS_DEVELOPER_TOKEN",
];

fn main() {
    let file = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../release.env");
    for var in VARS {
        println!("cargo::rerun-if-env-changed={var}");
    }
    let Ok(text) = std::fs::read_to_string(&file) else {
        return;
    };
    println!("cargo::rerun-if-changed={}", file.display());
    for line in text.lines() {
        let Some((key, value)) = line.trim().split_once('=') else {
            continue;
        };
        let key = key.trim().trim_start_matches("export ").trim();
        let value = value.trim().trim_matches(['"', '\'']);
        let set_in_env = std::env::var(key).is_ok_and(|v| !v.is_empty());
        if VARS.contains(&key) && !value.is_empty() && !set_in_env {
            println!("cargo::rustc-env={key}={value}");
        }
    }
}
