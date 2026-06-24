//! `bash` — Shell-Kommando mit Timeout, Cancellation, Prozess-Tree-Kill, `truncate_tail`.

use std::process::Stdio;
use std::time::Duration;

use async_trait::async_trait;
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::io::AsyncReadExt;
use tokio::process::Command;
use tokio_util::sync::CancellationToken;

use sepp_core::{Result, SeppError, ToolResult, ToolSpec};

use crate::truncate::{truncate_tail, DEFAULT_MAX_BYTES, DEFAULT_MAX_LINES};
use crate::{schema_for, Tool};

const DEFAULT_TIMEOUT_MS: u64 = 120_000;

/// Provider-Secrets, die nicht an Shell-Kommandos durchgereicht werden (Exfiltrationsschutz).
const SECRET_ENV_VARS: &[&str] = &[
    "ANTHROPIC_API_KEY",
    "OPENAI_API_KEY",
    "GOOGLE_API_KEY",
    "GEMINI_API_KEY",
];

#[derive(Debug, Deserialize, JsonSchema)]
struct BashParams {
    /// Auszuführendes Shell-Kommando (`sh -c`).
    command: String,
    /// Timeout in Millisekunden (Default 120000).
    #[serde(default)]
    timeout_ms: Option<u64>,
}

enum Outcome {
    Done(std::process::ExitStatus),
    Timeout,
    Cancelled,
}

/// Führt ein Shell-Kommando aus.
pub struct BashTool;

#[async_trait]
impl Tool for BashTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "bash".into(),
            label: "Bash".into(),
            description: "Führt ein Shell-Kommando via `sh -c` aus. Mit Timeout (Default 120s) \
                          und Prozess-Tree-Kill. stdout+stderr werden (vom Ende her) gekürzt."
                .into(),
            parameters: schema_for::<BashParams>(),
        }
    }

    async fn execute(
        &self,
        input: Value,
        cancel: CancellationToken,
        _on_update: Option<&(dyn Fn(ToolResult) + Send + Sync)>,
    ) -> Result<ToolResult> {
        let p: BashParams = serde_json::from_value(input)
            .map_err(|e| SeppError::Tool(format!("bash: ungültige Parameter: {e}")))?;
        let timeout = Duration::from_millis(p.timeout_ms.unwrap_or(DEFAULT_TIMEOUT_MS));

        let mut cmd = Command::new("sh");
        cmd.arg("-c")
            .arg(&p.command)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        // Provider-API-Keys nicht an (modell-gesteuerte) Shell-Kommandos durchreichen —
        // Schutz gegen Exfiltration via Prompt-Injection. (Built-in-Tools laufen nicht durch
        // die Extension-Sandbox; dies ist die gezielte Minimal-Absicherung.)
        for key in SECRET_ENV_VARS {
            cmd.env_remove(key);
        }

        // Eigene Prozessgruppe → kompletter Tree per killpg abbrechbar.
        #[cfg(unix)]
        unsafe {
            cmd.pre_exec(|| {
                if libc::setpgid(0, 0) != 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }

        let mut child = cmd
            .spawn()
            .map_err(|e| SeppError::Tool(format!("bash spawn: {e}")))?;
        let pid = child.id();

        // stdout/stderr nebenläufig vollständig lesen (verhindert Pipe-Deadlocks).
        let mut out_handle = child.stdout.take();
        let mut err_handle = child.stderr.take();
        let out_task = tokio::spawn(async move {
            let mut buf = Vec::new();
            if let Some(s) = out_handle.as_mut() {
                let _ = s.read_to_end(&mut buf).await;
            }
            buf
        });
        let err_task = tokio::spawn(async move {
            let mut buf = Vec::new();
            if let Some(s) = err_handle.as_mut() {
                let _ = s.read_to_end(&mut buf).await;
            }
            buf
        });

        let outcome = tokio::select! {
            status = child.wait() => match status {
                Ok(s) => Outcome::Done(s),
                Err(e) => return Err(SeppError::Tool(format!("bash wait: {e}"))),
            },
            _ = tokio::time::sleep(timeout) => Outcome::Timeout,
            _ = cancel.cancelled() => Outcome::Cancelled,
        };

        if matches!(outcome, Outcome::Timeout | Outcome::Cancelled) {
            kill_tree(pid, &mut child);
            let _ = child.wait().await; // Zombie ernten
        }

        let stdout_bytes = out_task.await.unwrap_or_default();
        let stderr_bytes = err_task.await.unwrap_or_default();

        // Bei Abbruch durch den Nutzer: harter Abbruch.
        if matches!(outcome, Outcome::Cancelled) {
            return Err(SeppError::Aborted);
        }

        let mut combined = String::from_utf8_lossy(&stdout_bytes).into_owned();
        let stderr_str = String::from_utf8_lossy(&stderr_bytes);
        if !stderr_str.is_empty() {
            if !combined.is_empty() && !combined.ends_with('\n') {
                combined.push('\n');
            }
            combined.push_str(&stderr_str);
        }

        let t = truncate_tail(&combined, DEFAULT_MAX_LINES, DEFAULT_MAX_BYTES);
        let mut content = t.content.clone();
        if let Some(note) = t.note() {
            content.push_str(&note);
        }

        let (is_error, exit_code, status_line) = match outcome {
            Outcome::Timeout => (
                true,
                None,
                Some(format!("[bash: Timeout nach {} ms]", timeout.as_millis())),
            ),
            Outcome::Done(s) => match s.code() {
                Some(0) => (false, Some(0), None),
                Some(c) => (true, Some(c), Some(format!("[exit code: {c}]"))),
                None => (true, None, Some("[durch Signal beendet]".into())),
            },
            // Bereits oben behandelt; dieser Arm hält den `match` ohne `panic!` exhaustiv
            // (AGENTS.md Rule #5: kein panic!/unreachable! in Library-Crates).
            Outcome::Cancelled => return Err(SeppError::Aborted),
        };

        if let Some(line) = status_line {
            if !content.is_empty() && !content.ends_with('\n') {
                content.push('\n');
            }
            content.push_str(&line);
        }
        if content.is_empty() {
            content.push_str("(keine Ausgabe)");
        }

        Ok(ToolResult {
            content: vec![sepp_core::ContentBlock::text(content)],
            details: json!({ "exit_code": exit_code, "command": p.command }),
            is_error,
        })
    }
}

#[cfg(unix)]
fn kill_tree(pid: Option<u32>, child: &mut tokio::process::Child) {
    if let Some(pid) = pid {
        // Ganze Prozessgruppe (negatives PID-Argument).
        unsafe {
            libc::kill(-(pid as i32), libc::SIGKILL);
        }
    } else {
        let _ = child.start_kill();
    }
}

#[cfg(not(unix))]
fn kill_tree(_pid: Option<u32>, child: &mut tokio::process::Child) {
    let _ = child.start_kill();
}

#[cfg(test)]
mod tests {
    use super::*;

    // current_thread: identisch zur Runtime, die die CLI nutzt.
    fn ct_runtime() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
    }

    fn run(cmd: &str, timeout_ms: Option<u64>) -> ToolResult {
        let rt = ct_runtime();
        rt.block_on(async {
            let tool = BashTool;
            let mut input = json!({ "command": cmd });
            if let Some(t) = timeout_ms {
                input["timeout_ms"] = json!(t);
            }
            tool.execute(input, CancellationToken::new(), None)
                .await
                .unwrap()
        })
    }

    #[test]
    fn echo_succeeds() {
        let r = run("echo hallo", None);
        assert!(!r.is_error);
        assert!(matches!(&r.content[0],
            sepp_core::ContentBlock::Text { text } if text.contains("hallo")));
        assert_eq!(r.details["exit_code"], 0);
    }

    #[test]
    fn nonzero_exit_is_error() {
        let r = run("exit 3", None);
        assert!(r.is_error);
        assert_eq!(r.details["exit_code"], 3);
    }

    #[test]
    fn timeout_kills_long_command_promptly() {
        let start = std::time::Instant::now();
        // Kindprozess, der ein Enkelkind (sleep) startet — Tree-Kill nötig.
        let r = run("sleep 30 & sleep 30", Some(300));
        assert!(r.is_error);
        // Muss klar vor den 30s zurückkehren.
        assert!(start.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn cancellation_aborts() {
        let rt = ct_runtime();
        rt.block_on(async {
            let tool = BashTool;
            let cancel = CancellationToken::new();
            let c2 = cancel.clone();
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_millis(150)).await;
                c2.cancel();
            });
            let res = tool
                .execute(json!({ "command": "sleep 30" }), cancel, None)
                .await;
            assert!(matches!(res, Err(SeppError::Aborted)));
        });
    }

    #[test]
    fn large_output_does_not_deadlock_on_current_thread() {
        // ~1 MB stdout über einen current_thread-Reactor: die nebenläufigen Reader-Tasks
        // müssen die Pipe leeren, während der Haupt-Task auf `child.wait()` wartet
        // (sonst Deadlock bei vollem Pipe-Puffer). Belegt die Runtime-Wahl der CLI.
        let r = run("seq 1 200000", Some(10_000));
        assert!(!r.is_error, "exit: {:?}", r.details["exit_code"]);
        assert!(matches!(&r.content[0],
            sepp_core::ContentBlock::Text { text } if text.contains("200000")));
    }
}
