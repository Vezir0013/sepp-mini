//! `bash` — Shell-Kommando mit Timeout, Cancellation, Prozess-Tree-Kill, `truncate_tail`.
//!
//! Im Hintergrund gestartete Prozesse (`cmd &`) erben die stdout/stderr-Pipe und halten sie
//! offen, solange sie leben. Das Tool wartet daher nach Exit des direkten Kindprozesses nur
//! eine kurze Drain-Frist (`POST_EXIT_DRAIN_MS`) und kehrt dann zurück, statt auf Pipe-EOF zu
//! blockieren.
//!
//! **Mit Guard** läuft `sh` in der OS-Sandbox des Agenten (Landlock/Seatbelt): Environment
//! geleert bis auf die Allowlist, Dateisystem auf die Agent-Policy begrenzt, TCP ohne `net`
//! verboten. Scheitert ein Kommando an der Sandbox („Permission denied"), bekommt das Modell
//! einen `[guard: …]`-Hinweis, wie der Mensch die Rechte erweitern kann.

use std::process::Stdio;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::process::Command;
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

use sepp_core::{Result, SeppError, ToolResult, ToolSpec};
use sepp_policy::{Action, Guard};

use crate::truncate::{truncate_tail, DEFAULT_MAX_BYTES, DEFAULT_MAX_LINES};
use crate::{authorize, schema_for, with_guard_details, Tool};

const DEFAULT_TIMEOUT_MS: u64 = 120_000;

/// Frist nach Exit des direkten Kindprozesses, in der noch gepufferte Ausgabe abgeholt wird,
/// bevor das Tool zurückkehrt — auch wenn ein im Hintergrund gestarteter Enkelprozess (`cmd &`)
/// die geerbte stdout/stderr-Pipe offen hält. Verhindert das Blockieren auf Pipe-EOF.
const POST_EXIT_DRAIN_MS: u64 = 1_000;

/// Obergrenze je Ausgabestrom **während** der Ausführung. `truncate_tail` greift erst auf dem
/// fertigen Puffer und kann den Host deshalb nicht schützen: `yes` oder `cat /dev/urandom`
/// füllen bis zum Timeout den Speicher, bevor überhaupt gekürzt wird. Großzügig über dem, was
/// am Ende behalten werden kann — es soll nur die unbegrenzte Aufnahme verhindern, nicht die
/// Trunkierung vorwegnehmen.
const MAX_CAPTURE_BYTES: usize = 8 * DEFAULT_MAX_BYTES;

/// Provider-Secrets, die nicht an Shell-Kommandos durchgereicht werden (Exfiltrationsschutz).
/// Mit Guard ist das Environment ohnehin Default-deny (Allowlist); diese Liste bleibt als
/// Defense-in-depth für `--mode yolo` und Tests ohne Guard.
pub const SECRET_ENV_VARS: &[&str] = &[
    "ANTHROPIC_API_KEY",
    "OPENAI_API_KEY",
    "GOOGLE_API_KEY",
    "GEMINI_API_KEY",
    "ZAI_API_KEY",
    "MOONSHOT_API_KEY",
];

/// Hinweis ans Modell, wenn ein Kommando unter Guard an der Sandbox scheitert.
const GUARD_HINT: &str = "[guard: Zugriff von der Sandbox verweigert; Rechte erweitern mit \
                          `sepp policy allow agent …` oder in .sepp/policy.toml]";

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

/// Aufgenommene Ausgabe eines Stroms, gedeckelt auf [`MAX_CAPTURE_BYTES`].
#[derive(Default)]
struct Capture {
    buf: Vec<u8>,
    /// Bytes, die wegen der Obergrenze verworfen wurden — für eine ehrliche Notiz am Ende.
    dropped: u64,
}

impl Capture {
    /// Hängt `chunk` an und wirft vorne weg, was über die Grenze läuft. Vorne, weil
    /// `truncate_tail` am Ende ohnehin das Ende behält.
    fn push(&mut self, chunk: &[u8]) {
        self.buf.extend_from_slice(chunk);
        // Erst beim Doppelten kompaktieren: ein `drain` je Chunk würde bei jedem 16-KiB-Block
        // den ganzen Puffer verschieben. So bleibt der Aufwand amortisiert O(1) je Byte.
        if self.buf.len() > 2 * MAX_CAPTURE_BYTES {
            let excess = self.buf.len() - MAX_CAPTURE_BYTES;
            self.buf.drain(..excess);
            self.dropped += excess as u64;
        }
    }

    /// Nimmt den Inhalt heraus und lässt die Zählung der verworfenen Bytes stehen.
    fn take(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.buf)
    }
}

/// Liest `reader` chunk-weise in `buf`, bis EOF/Fehler. Solange `stop` `false` ist, wird die
/// Ausgabe gesammelt; danach wird nur noch geleert (Pipe drainen, ohne den Puffer wachsen zu
/// lassen) — so bleibt ein im Hintergrund gestarteter Prozess lauffähig, ohne das Tool zu
/// blockieren. Die Ausgabe landet im geteilten `buf`.
fn spawn_reader<R>(
    reader: Option<R>,
    buf: Arc<Mutex<Capture>>,
    stop: Arc<AtomicBool>,
) -> tokio::task::JoinHandle<()>
where
    R: AsyncRead + Unpin + Send + 'static,
{
    tokio::spawn(async move {
        let Some(mut r) = reader else {
            return;
        };
        let mut chunk = [0u8; 16 * 1024];
        loop {
            match r.read(&mut chunk).await {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if !stop.load(Ordering::Relaxed) {
                        buf.lock().await.push(&chunk[..n]);
                    }
                }
            }
        }
    })
}

/// Führt ein Shell-Kommando aus — mit Guard in der OS-Sandbox des Agenten.
#[derive(Default)]
pub struct BashTool {
    guard: Option<Arc<Guard>>,
}

impl BashTool {
    pub fn new(guard: Option<Arc<Guard>>) -> Self {
        BashTool { guard }
    }
}

#[async_trait]
impl Tool for BashTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "bash".into(),
            label: "Bash".into(),
            description: "Führt ein Shell-Kommando via `sh -c` aus. Mit Timeout (Default 120s) \
                          und Prozess-Tree-Kill. stdout+stderr werden (vom Ende her) gekürzt. \
                          Im Hintergrund gestartete Prozesse (`&`) laufen nach Rückkehr weiter."
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
        // Guard: Rückfrage-Muster prüfen (Modus ask) und Audit — VOR dem Bauen des Kommandos.
        let authorized = authorize(
            self.guard.as_ref(),
            Action::Shell {
                command: p.command.clone(),
            },
        )
        .await?;

        let mut cmd = Command::new("sh");
        cmd.arg("-c")
            .arg(&p.command)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        // Provider-API-Keys nicht an (modell-gesteuerte) Shell-Kommandos durchreichen —
        // Schutz gegen Exfiltration via Prompt-Injection. Mit Guard leert die Sandbox das
        // Environment ohnehin (Allowlist); ohne Guard ist dies die Minimal-Absicherung.
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

        // Guard: Kindprozess in die OS-Sandbox sperren (Env-Allowlist, Landlock/Seatbelt) mit der
        // effektiven Agent-Policy plus evtl. einmaliger Zusatz-Gewährung.
        if let Some(g) = &self.guard {
            let policy = g.agent_spawn_policy(&authorized.auth.extra);
            g.prepare_process(&mut cmd, &policy)?;
        }

        let mut child = cmd
            .spawn()
            .map_err(|e| SeppError::Tool(format!("bash spawn: {e}")))?;
        let pid = child.id();

        // stdout/stderr nebenläufig in geteilte Puffer lesen (verhindert Pipe-Deadlocks bei
        // großer Ausgabe). `stop` schaltet die Reader nach dem Detachen auf reines Drainen um,
        // damit ein im Hintergrund weiterlaufender Prozess die Pipe nicht füllt.
        let out_buf = Arc::new(Mutex::new(Capture::default()));
        let err_buf = Arc::new(Mutex::new(Capture::default()));
        let stop = Arc::new(AtomicBool::new(false));
        let out_task = spawn_reader(child.stdout.take(), out_buf.clone(), stop.clone());
        let err_task = spawn_reader(child.stderr.take(), err_buf.clone(), stop.clone());

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

        // Restausgabe einsammeln — aber NICHT unbegrenzt auf Pipe-EOF warten: ein im Hintergrund
        // gestarteter Enkelprozess (`server &`) erbt die Pipe und hält sie offen, solange er
        // lebt. Nach kurzer Drain-Frist werden die noch laufenden Reader detacht (das Droppen der
        // JoinHandles im timeout-Block bricht sie nicht ab — sie drainen die Pipe weiter, damit
        // der Hintergrundprozess nicht blockiert); weitergearbeitet wird mit dem bis dahin
        // Gelesenen. Bei Timeout/Cancel ist der Tree bereits gekillt → EOF kommt sofort.
        let drained = tokio::time::timeout(Duration::from_millis(POST_EXIT_DRAIN_MS), async {
            let _ = out_task.await;
            let _ = err_task.await;
        })
        .await;
        if drained.is_err() {
            // Detachte Reader nicht mehr in die Puffer schreiben lassen (Speicher beschränken).
            stop.store(true, Ordering::Relaxed);
        }
        let (stdout_bytes, out_dropped) = {
            let mut g = out_buf.lock().await;
            (g.take(), g.dropped)
        };
        let (stderr_bytes, err_dropped) = {
            let mut g = err_buf.lock().await;
            (g.take(), g.dropped)
        };
        let dropped = out_dropped + err_dropped;

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
        if dropped > 0 {
            // Ohne diesen Satz nennt die Notiz oben eine Gesamtgröße, die nur den aufgenommenen
            // Teil zählt — der Rest ist schon während der Ausführung weggeworfen worden.
            content.push_str(&format!(
                "\n[bash: {dropped} weitere Bytes wurden während der Ausführung verworfen \
                 (Aufnahmegrenze {MAX_CAPTURE_BYTES} Bytes je Strom)]"
            ));
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
            // (Konvention: kein panic!/unreachable! in Library-Crates, siehe CLAUDE.md).
            Outcome::Cancelled => return Err(SeppError::Aborted),
        };

        if let Some(line) = status_line {
            if !content.is_empty() && !content.ends_with('\n') {
                content.push('\n');
            }
            content.push_str(&line);
        }
        // Unter Guard: eine Verweigerung durch die Sandbox als solche benennen, damit das Modell
        // nicht rät und der Mensch weiß, wo er freigeben kann. Unabhängig vom Exit-Code — ein
        // `cmd; echo $?` endet mit 0, die Verweigerung steht trotzdem in der Ausgabe.
        if self.guard.is_some() && looks_like_sandbox_denial(&combined) {
            if !content.is_empty() && !content.ends_with('\n') {
                content.push('\n');
            }
            content.push_str(GUARD_HINT);
        }
        if content.is_empty() {
            content.push_str("(keine Ausgabe)");
        }

        Ok(with_guard_details(
            ToolResult {
                content: vec![sepp_core::ContentBlock::text(content)],
                details: json!({ "exit_code": exit_code, "command": p.command }),
                is_error,
            },
            authorized.audit,
        ))
    }
}

/// Typische Meldungen, wenn Landlock/Seatbelt einen Zugriff verweigern — englisch (libc/dash)
/// und deutsch (coreutils mit `LANG=de_*`), dazu die errno-Namen.
fn looks_like_sandbox_denial(output: &str) -> bool {
    const MARKERS: &[&str] = &[
        "Permission denied",
        "Operation not permitted",
        "Keine Berechtigung",
        "Vorgang nicht zulässig",
        "Der Vorgang ist nicht erlaubt",
        "EACCES",
        "EPERM",
    ];
    MARKERS.iter().any(|m| output.contains(m))
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

    #[test]
    fn capture_keeps_small_output_verbatim() {
        let mut c = Capture::default();
        c.push(b"hallo ");
        c.push(b"welt");
        assert_eq!(c.take(), b"hallo welt");
        assert_eq!(c.dropped, 0);
    }

    #[test]
    fn capture_caps_growth_and_counts_dropped_bytes() {
        let mut c = Capture::default();
        let chunk = vec![b'x'; 16 * 1024];
        // Deutlich mehr als die Grenze einspeisen und dabei prüfen, dass der Puffer nie über
        // das Kompaktierungsfenster hinauswächst — das ist die eigentliche Zusage.
        let mut fed = 0u64;
        for _ in 0..200 {
            c.push(&chunk);
            fed += chunk.len() as u64;
            assert!(c.buf.len() <= 2 * MAX_CAPTURE_BYTES, "{}", c.buf.len());
        }
        assert!(c.dropped > 0);
        assert_eq!(c.buf.len() as u64 + c.dropped, fed);
    }

    #[test]
    fn capture_keeps_the_end_not_the_beginning() {
        let mut c = Capture::default();
        c.push(&vec![b'a'; 2 * MAX_CAPTURE_BYTES + 1]);
        c.push(b"ENDE");
        let out = c.take();
        assert!(out.ends_with(b"ENDE"));
    }

    fn run(cmd: &str, timeout_ms: Option<u64>) -> ToolResult {
        let rt = ct_runtime();
        rt.block_on(async {
            let tool = BashTool::default();
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
            let tool = BashTool::default();
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
    fn background_process_does_not_hang() {
        // Regression (Hänger an offener Pipe): Ein im Hintergrund gestarteter, langlebiger
        // Prozess (`sleep 30 &`) erbt die stdout/stderr-Pipe des Kommandos. Der direkte
        // Kindprozess (`sh`) endet sofort, die Pipe bleibt aber offen, solange der
        // Hintergrundprozess lebt. Das Tool darf NICHT auf Pipe-EOF warten, sondern muss nach
        // Kind-Exit + kurzer Drain-Frist mit der bis dahin gelesenen Ausgabe zurückkehren.
        let start = std::time::Instant::now();
        let r = run("echo started; sleep 30 &", Some(60_000));
        assert!(!r.is_error, "exit: {:?}", r.details["exit_code"]);
        assert!(
            matches!(&r.content[0],
                sepp_core::ContentBlock::Text { text } if text.contains("started")),
            "Ausgabe vor dem Backgrounding muss erhalten bleiben: {:?}",
            r.content[0]
        );
        // Muss lange vor den 30s des Hintergrundprozesses zurückkehren.
        assert!(
            start.elapsed() < Duration::from_secs(5),
            "Tool hing an der offenen Pipe (dauerte {:?})",
            start.elapsed()
        );
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

    #[test]
    fn sandbox_denial_markers_cover_english_and_german() {
        assert!(looks_like_sandbox_denial("cat: /x: Permission denied"));
        assert!(looks_like_sandbox_denial("cat: /x: Keine Berechtigung"));
        assert!(looks_like_sandbox_denial(
            "sh: 1: cannot create /x: Permission denied\nexit=2"
        ));
        assert!(looks_like_sandbox_denial("mount: Operation not permitted"));
        assert!(!looks_like_sandbox_denial("hello.txt\nnotes.txt"));
    }

    #[test]
    fn blacklist_contains_all_provider_keys() {
        for k in [
            "ANTHROPIC_API_KEY",
            "OPENAI_API_KEY",
            "ZAI_API_KEY",
            "MOONSHOT_API_KEY",
        ] {
            assert!(SECRET_ENV_VARS.contains(&k), "{k} fehlt in SECRET_ENV_VARS");
        }
    }

    #[test]
    fn guard_scrubs_env_and_records_audit() {
        // NullSandbox: keine FS-Grenze, aber Env-Allowlist greift (Default-deny) und der
        // Audit-Eintrag landet in details["guard"].
        let dir = tempfile::tempdir().unwrap();
        let guard = crate::test_support::guard_for(dir.path(), sepp_policy::Mode::Auto);
        let rt = ct_runtime();
        let r = rt.block_on(async {
            std::env::set_var("SEPP_TEST_SECRET", "geheim");
            BashTool::new(Some(guard))
                .execute(
                    json!({ "command": "echo \"[$SEPP_TEST_SECRET][$PATH]\"" }),
                    CancellationToken::new(),
                    None,
                )
                .await
                .unwrap()
        });
        let text = match &r.content[0] {
            sepp_core::ContentBlock::Text { text } => text.clone(),
            other => panic!("Text erwartet: {other:?}"),
        };
        assert!(
            text.starts_with("[]["),
            "Secret darf nicht durchkommen: {text}"
        );
        assert!(!text.starts_with("[][]"), "PATH muss durchkommen: {text}");
        assert_eq!(r.details["guard"]["decision"], "allow");
    }

    /// Gated wie die Landlock-Tests in sepp-policy: braucht durchsetzbares Landlock.
    #[test]
    #[ignore = "braucht durchsetzbares Landlock"]
    fn bash_under_guard_writes_inside_allowed_dir_and_not_outside() {
        use sepp_policy::{
            default_sandbox, AgentSection, BuiltinDefaults, Grants, Guard, Mode, PolicyFile,
            PolicySet, ResolveCtx, Source,
        };
        let inside = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let inside_c = inside.path().canonicalize().unwrap();
        let file = PolicyFile {
            mode: Some(Mode::Auto),
            agent: Some(AgentSection {
                grants: Grants {
                    fs_read: vec![inside_c.display().to_string(), "system".into()],
                    fs_write: vec![inside_c.display().to_string()],
                    ..Grants::default()
                },
                ..AgentSection::default()
            }),
            ..PolicyFile::default()
        };
        let ctx = ResolveCtx {
            home: None,
            cwd: inside_c.clone(),
            tmpdir: inside_c.clone(),
        };
        let set = PolicySet::merge(
            vec![(Source::File("/t.toml".into()), file)],
            &BuiltinDefaults::default(),
            None,
            &ctx,
        );
        let guard = Arc::new(Guard::new(set, default_sandbox()));
        let rt = ct_runtime();
        let (ok, bad) = rt.block_on(async {
            let tool = BashTool::new(Some(guard));
            let ok = tool
                .execute(
                    json!({ "command": format!("echo hi > '{}/ok.txt'", inside_c.display()) }),
                    CancellationToken::new(),
                    None,
                )
                .await
                .unwrap();
            let bad = tool
                .execute(
                    json!({ "command": format!("echo hi > '{}/escaped.txt'", outside.path().display()) }),
                    CancellationToken::new(),
                    None,
                )
                .await
                .unwrap();
            (ok, bad)
        });
        assert!(!ok.is_error, "{:?}", ok.content);
        assert!(inside_c.join("ok.txt").exists());
        assert!(bad.is_error);
        assert!(!outside.path().join("escaped.txt").exists());
        let text = match &bad.content[0] {
            sepp_core::ContentBlock::Text { text } => text.clone(),
            other => panic!("Text erwartet: {other:?}"),
        };
        assert!(text.contains("[guard:"), "Hinweis fehlt: {text}");
    }
}
