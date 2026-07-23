//! Env-scrubbed, capped, timeout-bounded command runner.

use std::time::Duration;

use tokio::io::AsyncReadExt;
use tokio::process::Command;

use crate::Jail;

pub struct TermOutcome {
    pub exit_code: i32,
    pub output: String,
}

fn name_is_secret(name: &str) -> bool {
    let up = name.to_uppercase();
    up.contains("KEY")
        || up.contains("TOKEN")
        || up.contains("SECRET")
        || up.contains("PASSWORD")
        || up.contains("CREDENTIAL")
}

pub async fn terminal(
    jail: &Jail,
    command: &str,
    scrub_names: &[String],
    timeout_secs: u64,
    max_bytes: usize,
) -> anyhow::Result<TermOutcome> {
    let mut cmd = Command::new("sh");
    cmd.arg("-c").arg(command).current_dir(jail.root());
    cmd.stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    for (name, _) in std::env::vars() {
        if name_is_secret(&name) || scrub_names.iter().any(|s| s == &name) {
            cmd.env_remove(&name);
        }
    }
    let mut child = cmd.spawn()?;
    let mut stdout = child.stdout.take().unwrap();
    let mut stderr = child.stderr.take().unwrap();
    let so_task = tokio::spawn(async move {
        let mut buf = Vec::new();
        let _ = stdout.read_to_end(&mut buf).await;
        buf
    });
    let se_task = tokio::spawn(async move {
        let mut buf = Vec::new();
        let _ = stderr.read_to_end(&mut buf).await;
        buf
    });
    let waited = tokio::time::timeout(Duration::from_secs(timeout_secs), async {
        let status = child.wait().await;
        let so = so_task.await.unwrap_or_default();
        let se = se_task.await.unwrap_or_default();
        (status, so, se)
    })
    .await;
    match waited {
        Ok((status, so, se)) => {
            let mut merged = so;
            merged.extend_from_slice(&se);
            let exit_code = status.map(|s| s.code().unwrap_or(-1)).unwrap_or(-1);
            if merged.len() <= max_bytes {
                let output = String::from_utf8_lossy(&merged).into_owned();
                Ok(TermOutcome { exit_code, output })
            } else {
                let mut s = String::from_utf8_lossy(&merged[..max_bytes]).into_owned();
                s.push_str("\n[truncated]");
                Ok(TermOutcome { exit_code, output: s })
            }
        }
        Err(_) => {
            let _ = child.kill().await;
            Ok(TermOutcome {
                exit_code: -1,
                output: format!("killed: timeout after {timeout_secs}s"),
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn terminal_runs_scrubs_and_caps() {
        let d = tempfile::tempdir().unwrap();
        let jail = crate::Jail::new(d.path()).unwrap();
        std::env::set_var("FC_TEST_SECRET_VAL", "sk-hide-me");
        let o = terminal(&jail, "echo -n \"v=${FC_TEST_SECRET_VAL:-scrubbed}\"", &[], 10, 4096).await.unwrap();
        assert_eq!(o.exit_code, 0);
        assert_eq!(o.output, "v=scrubbed"); // name matches SECRET -> scrubbed
        let o = terminal(&jail, "yes x | head -c 100000", &[], 10, 1000).await.unwrap();
        assert!(o.output.len() < 1100 && o.output.contains("[truncated"));
        let o = terminal(&jail, "sleep 30", &[], 1, 4096).await.unwrap();
        assert_eq!(o.exit_code, -1);
        std::env::remove_var("FC_TEST_SECRET_VAL");
    }
}
