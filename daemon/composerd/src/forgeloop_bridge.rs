//! forgeloop bridge — runs journal gates by subprocess and copies verdicts
//! from journals on disk. Read-only toward the forgeloop tree (design D8);
//! there is deliberately NO code path here that can invoke mint.sh.

use std::path::Path;
use std::time::Duration;

use tokio::io::AsyncReadExt;
use tokio::process::Command;

pub struct GateOutcome {
    /// `Some` only when the gate printed journal evidence AND the journal
    /// parsed to a decision line. NEVER fabricated.
    pub verdict: Option<GateVerdict>,
    pub exit_code: i32,
    /// Capped combined stdout+stderr for the tool result / error event.
    pub output: String,
}

pub struct GateVerdict {
    pub decision: String,
    pub intent: String,
    pub journal_path: std::path::PathBuf, // absolute
}

/// Mirror of `tools::terminal`'s secret-name filter (that helper is private;
/// the tools crate's public surface is unchanged). A var is scrubbed when its
/// uppercased name contains one of the secret markers OR it is listed in
/// `scrub_names` (the configured `api_key_env` names).
fn name_is_secret(name: &str, scrub_names: &[String]) -> bool {
    let up = name.to_uppercase();
    up.contains("KEY")
        || up.contains("TOKEN")
        || up.contains("SECRET")
        || up.contains("PASSWORD")
        || up.contains("CREDENTIAL")
        || scrub_names.iter().any(|s| s == name)
}

/// Run `bash <dir>/harness/journal-gate.sh <target>` with a scrubbed env,
/// parse the LAST `evidence: <path>.jsonl` occurrence from stdout, read that
/// journal (relative paths resolved against <dir>), and extract the last
/// JSONL line carrying a "decision" key. NEVER fabricates a verdict: any
/// parse/read miss yields verdict=None.
pub async fn run_gate(
    dir: &Path,
    target: &str,
    scrub_names: &[String],
    timeout_secs: u64,
    cap: usize,
) -> anyhow::Result<GateOutcome> {
    if target.is_empty()
        || !target
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
        || target.starts_with('-')
    {
        anyhow::bail!("invalid gate target: {target:?}");
    }
    let script = dir.join("harness/journal-gate.sh");
    if !script.exists() {
        anyhow::bail!("no journal-gate.sh under {}", dir.display());
    }

    let parent_env: Vec<(String, String)> = std::env::vars()
        .filter(|(k, _)| !name_is_secret(k, scrub_names))
        .collect();

    let mut cmd = Command::new("bash");
    cmd.arg(&script)
        .arg(target)
        .current_dir(dir)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .env_clear();
    for (k, v) in &parent_env {
        cmd.env(k, v);
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

    let timed_out = tokio::select! {
        biased;
        _ = tokio::time::sleep(Duration::from_secs(timeout_secs)) => true,
        status = child.wait() => {
            let exit_code = status.map(|s| s.code().unwrap_or(-1)).unwrap_or(-1);
            let so = so_task.await.unwrap_or_default();
            let se = se_task.await.unwrap_or_default();
            let stdout_str = String::from_utf8_lossy(&so).into_owned();
            let stderr_str = String::from_utf8_lossy(&se).into_owned();
            let verdict = parse_evidence(dir, &stdout_str).and_then(|jp| read_verdict(&jp));
            let output = cap_combined(&stdout_str, &stderr_str, cap);
            return Ok(GateOutcome { verdict, exit_code, output });
        }
    };

    if timed_out {
        let _ = child.kill().await;
        let so = so_task.await.unwrap_or_default();
        let se = se_task.await.unwrap_or_default();
        let stdout_str = String::from_utf8_lossy(&so).into_owned();
        let stderr_str = String::from_utf8_lossy(&se).into_owned();
        let mut output = cap_combined(&stdout_str, &stderr_str, cap);
        output.push_str(" [timeout]");
        return Ok(GateOutcome { verdict: None, exit_code: -1, output });
    }

    unreachable!()
}

/// Scan stdout for the LAST `evidence: <token>` line and return the resolved
/// journal path (relative paths resolved against `dir`).
fn parse_evidence(dir: &Path, stdout: &str) -> Option<std::path::PathBuf> {
    let mut found: Option<String> = None;
    for line in stdout.lines() {
        if let Some(rest) = line.find("evidence: ") {
            let after = &line[rest + "evidence: ".len()..];
            // Token is whitespace-terminated; require a .jsonl suffix.
            let tok_end = after
                .find(|c: char| c.is_whitespace())
                .unwrap_or(after.len());
            let tok = &after[..tok_end];
            if tok.ends_with(".jsonl") && !tok.is_empty() {
                found = Some(tok.to_string());
            }
        }
    }
    let tok = found?;
    let p = std::path::PathBuf::from(&tok);
    let resolved = if p.is_absolute() {
        p
    } else {
        dir.join(&p)
    };
    // Canonicalize to an absolute path (also confirms existence).
    std::fs::canonicalize(&resolved).ok()
}

/// Read the journal and extract the LAST line carrying a "decision" string
/// key. `intent` defaults to "" when absent. Any read/parse miss -> None.
fn read_verdict(journal_path: &Path) -> Option<GateVerdict> {
    let text = std::fs::read_to_string(journal_path).ok()?;
    let mut decision: Option<String> = None;
    let mut intent = String::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let v: serde_json::Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(_) => continue,
        };
        if let Some(d) = v.get("decision").and_then(|d| d.as_str()) {
            decision = Some(d.to_string());
            if let Some(i) = v.get("intent").and_then(|i| i.as_str()) {
                intent = i.to_string();
            } else {
                intent.clear();
            }
        }
    }
    let decision = decision?;
    Some(GateVerdict {
        decision,
        intent,
        journal_path: journal_path.to_path_buf(),
    })
}

fn cap_combined(stdout: &str, stderr: &str, cap: usize) -> String {
    let mut merged = String::with_capacity(stdout.len() + stderr.len() + 1);
    merged.push_str(stdout);
    if !stderr.is_empty() {
        if !merged.is_empty() && !merged.ends_with('\n') {
            merged.push('\n');
        }
        merged.push_str(stderr);
    }
    if merged.len() <= cap {
        merged
    } else {
        let mut s = merged[..cap].to_string();
        s.push_str("\n[truncated]");
        s
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(dir: &std::path::Path, green: bool) {
        let harness = dir.join("harness");
        std::fs::create_dir_all(&harness).unwrap();
        std::fs::create_dir_all(dir.join("runs")).unwrap();
        let script = if green {
            r#"#!/usr/bin/env bash
mkdir -p runs
printf '%s\n' '{"schema":"x","decision":"fail","intent":"earlier iteration"}' > runs/t1.jsonl
printf '%s\n' '{"schema":"x","decision":"pass","intent":"demo intent"}' >> runs/t1.jsonl
echo "GATE GREEN: '$1' — evidence: runs/t1.jsonl (decision:pass, 0s fresh)"
"#
        } else {
            r#"#!/usr/bin/env bash
echo "GATE RED: '$1' — no green journal" >&2
exit 1
"#
        };
        std::fs::write(harness.join("journal-gate.sh"), script).unwrap();
    }

    #[tokio::test]
    async fn green_gate_yields_pointer_copied_verdict() {
        let d = tempfile::tempdir().unwrap();
        fixture(d.path(), true);
        let out = run_gate(d.path(), "demo-app", &[], 30, 65536).await.unwrap();
        assert_eq!(out.exit_code, 0);
        let v = out.verdict.expect("verdict");
        assert_eq!(v.decision, "pass"); // LAST decision line wins
        assert_eq!(v.intent, "demo intent");
        assert!(v.journal_path.is_absolute());
        assert!(v.journal_path.exists());
    }

    #[tokio::test]
    async fn red_gate_without_evidence_never_fabricates() {
        let d = tempfile::tempdir().unwrap();
        fixture(d.path(), false);
        let out = run_gate(d.path(), "bad-app", &[], 30, 65536).await.unwrap();
        assert_ne!(out.exit_code, 0);
        assert!(out.verdict.is_none(), "no journal, no verdict — ever");
        assert!(out.output.contains("GATE RED"));
    }

    #[tokio::test]
    async fn hostile_target_is_rejected() {
        let d = tempfile::tempdir().unwrap();
        fixture(d.path(), true);
        assert!(run_gate(d.path(), "x; rm -rf /", &[], 30, 65536).await.is_err());
        assert!(run_gate(d.path(), "../escape", &[], 30, 65536).await.is_err());
    }
}
