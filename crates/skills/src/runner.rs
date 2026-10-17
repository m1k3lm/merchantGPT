//! Runs a skill script as a child process with a timeout, an output cap and a
//! minimal environment, so secrets in the app's environment do not leak.

use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use tokio::io::AsyncReadExt;
use tokio::process::Command;

#[derive(Debug, Clone, Copy)]
pub struct ScriptLimits {
    pub timeout: Duration,
    pub max_output_bytes: usize,
}

impl Default for ScriptLimits {
    fn default() -> Self {
        Self {
            timeout: Duration::from_secs(60),
            max_output_bytes: 20_000,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScriptOutput {
    pub status: Option<i32>,
    pub stdout: String,
    pub stderr: String,
    pub timed_out: bool,
    pub truncated: bool,
}

const PASSTHROUGH_ENV: &[&str] = &["PATH", "HOME", "LANG", "TMPDIR", "SystemRoot", "TEMP"];

fn command_for(script: &Path) -> Command {
    let ext = script.extension().and_then(|e| e.to_str()).unwrap_or("");
    let mut cmd = match ext {
        "sh" => {
            let mut c = Command::new("bash");
            c.arg(script);
            c
        }
        "py" => {
            let mut c = Command::new("python3");
            c.arg("-I").arg(script);
            c
        }
        _ => Command::new(script),
    };
    cmd.env_clear();
    for k in PASSTHROUGH_ENV {
        if let Ok(v) = std::env::var(k) {
            cmd.env(k, v);
        }
    }
    cmd
}

async fn read_capped<R: AsyncReadExt + Unpin>(mut r: R, cap: usize) -> (Vec<u8>, bool) {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 8192];
    let mut truncated = false;
    loop {
        match r.read(&mut chunk).await {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                let room = cap.saturating_sub(buf.len());
                buf.extend_from_slice(&chunk[..n.min(room)]);
                truncated |= n > room;
            }
        }
    }
    (buf, truncated)
}

/// `script` must already be resolved to a file inside the skill's `scripts`
/// folder; `cwd` is the skill folder.
pub async fn run_script(
    script: &Path,
    args: &[String],
    cwd: &Path,
    limits: ScriptLimits,
) -> std::io::Result<ScriptOutput> {
    let mut child = command_for(script)
        .args(args)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()?;
    let stdout = child.stdout.take().expect("piped");
    let stderr = child.stderr.take().expect("piped");
    let cap = limits.max_output_bytes;
    let run = async {
        let (out, err) = tokio::join!(read_capped(stdout, cap), read_capped(stderr, cap));
        let status = child.wait().await;
        (out, err, status)
    };
    match tokio::time::timeout(limits.timeout, run).await {
        Ok(((out, t1), (err, t2), status)) => Ok(ScriptOutput {
            status: status?.code(),
            stdout: String::from_utf8_lossy(&out).into_owned(),
            stderr: String::from_utf8_lossy(&err).into_owned(),
            timed_out: false,
            truncated: t1 || t2,
        }),
        // Dropping the future drops the child, which kill_on_drop terminates.
        Err(_) => Ok(ScriptOutput {
            status: None,
            stdout: String::new(),
            stderr: String::new(),
            timed_out: true,
            truncated: false,
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn script(dir: &Path, name: &str, body: &str) -> std::path::PathBuf {
        let p = dir.join(name);
        std::fs::write(&p, body).unwrap();
        p
    }

    #[tokio::test]
    async fn script_runs_with_timeout_and_output_cap() {
        let dir = tempfile::tempdir().unwrap();
        std::env::set_var("MERCHANTGPT_TEST_SECRET", "leak");
        let echo = script(
            dir.path(),
            "echo.sh",
            "echo \"args:$1,$2 cwd:$(basename \"$PWD\") secret:${MERCHANTGPT_TEST_SECRET:-none}\"; echo oops >&2; exit 3",
        );
        let out = run_script(
            &echo,
            &["a b".into(), "c".into()],
            dir.path(),
            ScriptLimits::default(),
        )
        .await
        .unwrap();
        let cwd_name = dir.path().file_name().unwrap().to_str().unwrap();
        assert_eq!(
            out.stdout.trim(),
            format!("args:a b,c cwd:{cwd_name} secret:none")
        );
        assert_eq!(out.stderr.trim(), "oops");
        assert_eq!(out.status, Some(3));

        let noisy = script(dir.path(), "noisy.sh", "yes x | head -c 100000");
        let out = run_script(
            &noisy,
            &[],
            dir.path(),
            ScriptLimits {
                timeout: Duration::from_secs(10),
                max_output_bytes: 1000,
            },
        )
        .await
        .unwrap();
        assert_eq!(out.stdout.len(), 1000);
        assert!(out.truncated);

        let slow = script(dir.path(), "slow.sh", "sleep 30");
        let started = std::time::Instant::now();
        let out = run_script(
            &slow,
            &[],
            dir.path(),
            ScriptLimits {
                timeout: Duration::from_millis(200),
                max_output_bytes: 1000,
            },
        )
        .await
        .unwrap();
        assert!(out.timed_out);
        assert!(started.elapsed() < Duration::from_secs(5));
    }
}
