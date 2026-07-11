//! Interaktive TUI (ratatui/crossterm): Chat-Verlauf, Live-Streaming, Slash-Commands,
//! Baum-Navigation (`/tree`) und Session-Auswahl (`/resume`).
//!
//! Nebenläufigkeit: der Agent-`prompt`/`compact` läuft als Task hinter einem `Mutex`; er
//! streamt `AgentEvent`s über einen Channel an die UI-Schleife. Die UI hält eine eigene
//! Transcript-Kopie und sperrt den Store nur im Leerlauf (für `/tree` etc.) — so blockiert
//! Streaming nie das Rendering. Gezeichnet wird per Doppelpuffer-Diff (kein Flackern).

use std::io::{self, Stdout};
use std::sync::Arc;

use anyhow::Result;
use crossterm::event::{Event, EventStream, KeyCode, KeyEventKind, KeyModifiers};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use futures::StreamExt;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::Line;
use ratatui::widgets::{Block, Borders, Paragraph};
use ratatui::{Frame, Terminal};
use tokio::sync::{mpsc, Mutex};
use tokio_util::sync::CancellationToken;

use sepp_agent::resources::ResourceSet;
use sepp_agent::{AgentEvent, AgentSession};
use sepp_core::{ContentBlock, Message, Role};
use sepp_hooks::{HookHost, RhaiHookHost};
use sepp_provider::models;
use sepp_session::{SessionInfo, SessionStore};

use crate::session;

type Term = Terminal<CrosstermBackend<Stdout>>;

#[derive(Clone, Copy, PartialEq, Debug)]
enum Kind {
    User,
    Assistant,
    Thinking,
    Info,
    Error,
}

struct DisplayMsg {
    kind: Kind,
    text: String,
}

enum Mode {
    Chat,
    Tree,
    Resume,
}

enum UiMsg {
    Agent(AgentEvent),
    Done(Option<String>),
}

/// Sendet beim Drop ein `Done`, falls nicht entschärft — so bleibt die UI nie im Zustand
/// „läuft noch" hängen, selbst wenn die Task entlädt (Debug-Panic). Im Release (`panic=abort`)
/// läuft Drop zwar nicht, aber dort beendet der Panic den Prozess (Panic-Hook stellt das
/// Terminal wieder her).
struct DoneOnDrop {
    tx: mpsc::UnboundedSender<UiMsg>,
    armed: bool,
}

impl DoneOnDrop {
    fn new(tx: mpsc::UnboundedSender<UiMsg>) -> Self {
        DoneOnDrop { tx, armed: true }
    }
    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for DoneOnDrop {
    fn drop(&mut self) {
        if self.armed {
            let _ = self
                .tx
                .send(UiMsg::Done(Some("Task unerwartet beendet".into())));
        }
    }
}

struct TreeLine {
    id: String,
    display: String,
}

struct App {
    session: Arc<Mutex<AgentSession>>,
    transcript: Vec<DisplayMsg>,
    streaming: Option<String>,
    streaming_thinking: Option<String>,
    show_thinking: bool,
    show_status: bool,
    input: String,
    status: String,
    running: bool,
    cancel: Option<CancellationToken>,
    scroll_back: u16,
    mode: Mode,
    tree_lines: Vec<TreeLine>,
    tree_sel: usize,
    resume_list: Vec<SessionInfo>,
    resume_sel: usize,
    prompt_templates: Vec<(String, String)>,
    base_prompt: String,
    should_quit: bool,
    /// Feedback-/Startup-Zeilen AUSSERHALB des Transcripts, gerendert unter dem Chatverlauf:
    /// überleben so rebuild_transcript (das nur aus Session-Messages baut) und gelten bis zur
    /// nächsten Nutzeraktion (start_prompt/start_compact bzw. Kontextwechsel leeren sie).
    notices: Vec<DisplayMsg>,
    tx: mpsc::UnboundedSender<UiMsg>,
}

/// Startet die TUI und blockiert, bis der Nutzer beendet. `startup_notices` sind Hinweise aus
/// dem CLI-Start (z. B. „--think wirkungslos", Cross-Provider-Modellwarnung), die im Chatfenster
/// erscheinen müssen — ein eprintln verpufft hinter dem Alternate-Screen.
pub async fn run(
    agent: AgentSession,
    prompt_templates: Vec<(String, String)>,
    base_prompt: String,
    show_thinking: bool,
    startup_notices: Vec<String>,
) -> Result<()> {
    install_panic_hook();
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let mut terminal = Terminal::new(CrosstermBackend::new(stdout))?;

    let (tx, mut rx) = mpsc::unbounded_channel();
    let mut app = App::new(
        Arc::new(Mutex::new(agent)),
        tx,
        prompt_templates,
        base_prompt,
        show_thinking,
    );
    app.rebuild_transcript().await;
    // Start-Hinweise als Notices (nicht ins Transcript): rebuild_transcript baut den Verlauf
    // bei jedem Turn-Ende aus den Session-Messages neu — dort eingefügte Zeilen verschwänden
    // rückwirkend. Notices leben, bis der Nutzer die nächste Aktion startet.
    for text in startup_notices {
        app.notices.push(DisplayMsg {
            kind: Kind::Info,
            text,
        });
    }
    // Prompt-Templates, die Builtins verschatten, sind per Slash unerreichbar — sichtbar warnen.
    let shadowed = template_collisions(&app.prompt_templates);
    if !shadowed.is_empty() {
        app.notices.push(DisplayMsg {
            kind: Kind::Info,
            text: format!(
                "Hinweis: Prompt-Template(s) {} kollidieren mit Builtin-Befehlen und sind \
                 per / nicht aufrufbar.",
                slash_list(&shadowed)
            ),
        });
    }

    let mut events = EventStream::new();
    let result = loop {
        if let Err(e) = terminal.draw(|f| app.render(f)) {
            break Err(e.into());
        }
        tokio::select! {
            maybe = events.next() => match maybe {
                Some(Ok(ev)) => app.on_term_event(ev).await,
                Some(Err(e)) => break Err(e.into()),
                None => {}
            },
            Some(msg) = rx.recv() => app.on_ui_msg(msg).await,
        }
        if app.should_quit {
            break Ok(());
        }
    };

    restore(&mut terminal);

    // Konversation abschließen: Session fsync'en. Ein evtl. laufender, beim Quit gecancelter
    // Prompt-Task gibt den Lock frei → `lock().await` wartet sauber.
    {
        let mut g = app.session.lock().await;
        if let Err(e) = g.finalize().await {
            eprintln!("Hinweis: Session-Abschluss fehlgeschlagen: {e}");
        }
    }
    result
}

fn restore(terminal: &mut Term) {
    let _ = disable_raw_mode();
    let _ = execute!(terminal.backend_mut(), LeaveAlternateScreen);
    let _ = terminal.show_cursor();
}

/// Stellt das Terminal auch bei einem Panic wieder her (sonst bleibt es im Raw-/Alt-Screen-
/// Modus „kaputt"). Der ursprüngliche Hook (Backtrace etc.) läuft danach normal weiter.
fn install_panic_hook() {
    let original = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = disable_raw_mode();
        let _ = execute!(io::stdout(), LeaveAlternateScreen);
        original(info);
    }));
}

impl App {
    fn new(
        session: Arc<Mutex<AgentSession>>,
        tx: mpsc::UnboundedSender<UiMsg>,
        prompt_templates: Vec<(String, String)>,
        base_prompt: String,
        show_thinking: bool,
    ) -> Self {
        App {
            session,
            transcript: Vec::new(),
            streaming: None,
            streaming_thinking: None,
            show_thinking,
            show_status: true,
            input: String::new(),
            status: "bereit · /help für Befehle".into(),
            running: false,
            cancel: None,
            scroll_back: 0,
            mode: Mode::Chat,
            tree_lines: Vec::new(),
            tree_sel: 0,
            resume_list: Vec::new(),
            resume_sel: 0,
            prompt_templates,
            base_prompt,
            should_quit: false,
            notices: Vec::new(),
            tx,
        }
    }

    async fn rebuild_transcript(&mut self) {
        let g = self.session.lock().await;
        self.transcript = transcript_from_messages(g.messages(), self.show_thinking);
        self.status = idle_status(&g);
    }

    /// Meldung in die Statuszeile; ist sie versteckt (`/hide`), zusätzlich als Notice unter den
    /// Chatverlauf — Feedback darf nie unsichtbar verpuffen (verworfene Eingaben, Befehls-
    /// Ausgaben, Fehler). Notices statt Transcript, weil rebuild_transcript den Verlauf bei
    /// jedem Turn-Ende komplett aus den Session-Messages neu baut und Ad-hoc-Zeilen darin
    /// rückwirkend verschwänden; der Scroll-Reset holt die View ans Ende, wo die Notice steht.
    fn notify_kind(&mut self, kind: Kind, text: String) {
        if !self.show_status {
            self.notices.push(DisplayMsg {
                kind,
                text: text.clone(),
            });
            self.scroll_back = 0;
        }
        self.status = text;
    }

    /// [`Self::notify_kind`] als Info (grau).
    fn notify(&mut self, text: impl Into<String>) {
        self.notify_kind(Kind::Info, text.into());
    }

    /// [`Self::notify_kind`] als Fehler (rot).
    fn notify_error(&mut self, text: impl Into<String>) {
        self.notify_kind(Kind::Error, text.into());
    }

    // ---- Eingabe ---------------------------------------------------------

    async fn on_term_event(&mut self, ev: Event) {
        let Event::Key(k) = ev else { return };
        if k.kind == KeyEventKind::Release {
            return;
        }
        let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
        if ctrl && matches!(k.code, KeyCode::Char('c') | KeyCode::Char('d')) {
            if let Some(c) = self.cancel.take() {
                c.cancel();
            }
            self.should_quit = true;
            return;
        }
        match self.mode {
            Mode::Chat => self.on_chat_key(k.code).await,
            Mode::Tree => self.on_tree_key(k.code).await,
            Mode::Resume => self.on_resume_key(k.code).await,
        }
    }

    async fn on_chat_key(&mut self, code: KeyCode) {
        match code {
            KeyCode::Enter => self.submit().await,
            KeyCode::Backspace => {
                self.input.pop();
            }
            KeyCode::Char(c) => self.input.push(c),
            KeyCode::Esc => {
                if let Some(c) = self.cancel.take() {
                    c.cancel();
                    self.status = "Abbruch …".into();
                } else {
                    self.input.clear();
                }
            }
            KeyCode::PageUp => self.scroll_back = self.scroll_back.saturating_add(10),
            KeyCode::PageDown => self.scroll_back = self.scroll_back.saturating_sub(10),
            KeyCode::Up => self.scroll_back = self.scroll_back.saturating_add(1),
            KeyCode::Down => self.scroll_back = self.scroll_back.saturating_sub(1),
            _ => {}
        }
    }

    async fn submit(&mut self) {
        let text = self.input.trim().to_string();
        if text.is_empty() {
            return;
        }
        if let Some(cmd) = text.strip_prefix('/') {
            // Eingabe erst nach ANNAHME leeren — ein wegen laufendem Turn abgewiesener Befehl
            // (inkl. Prompt-Template mit Argumenten) bleibt zum erneuten Absenden stehen,
            // genau wie der Prompt-Pfad darunter.
            if self.handle_command(cmd).await {
                self.input.clear();
            }
            return;
        }
        if self.running {
            // Eingabe NICHT verwerfen — der getippte Prompt bleibt zum Absenden stehen,
            // sobald der laufende Turn fertig ist.
            self.notify("läuft noch — bitte warten");
            return;
        }
        self.input.clear();
        self.transcript.push(DisplayMsg {
            kind: Kind::User,
            text: text.clone(),
        });
        self.scroll_back = 0;
        self.start_prompt(text);
    }

    fn start_prompt(&mut self, text: String) {
        // Neue Nutzeraktion: bisheriges Notice-Feedback hat seinen Zweck erfüllt.
        self.notices.clear();
        self.running = true;
        self.status = "denkt …".into();
        let cancel = CancellationToken::new();
        self.cancel = Some(cancel.clone());
        let sess = self.session.clone();
        let tx = self.tx.clone();
        let tx_ev = tx.clone();
        tokio::spawn(async move {
            let mut guard = DoneOnDrop::new(tx.clone());
            // Lock nur für die Dauer von prompt() halten und VOR dem Done-Signal freigeben,
            // damit die UI (rebuild_transcript) sofort locken kann.
            let res = {
                let mut g = sess.lock().await;
                let on_event = move |ev: AgentEvent| {
                    let _ = tx_ev.send(UiMsg::Agent(ev));
                };
                g.prompt(&text, &on_event, cancel).await
            };
            guard.disarm();
            let _ = tx.send(UiMsg::Done(res.err().map(|e| e.to_string())));
        });
    }

    // ---- Slash-Commands --------------------------------------------------

    /// Führt einen Slash-Befehl aus. `false` = abgewiesen („läuft noch — bitte warten"), die
    /// Eingabe soll dann zum erneuten Absenden stehen bleiben (Parität zum Prompt-Pfad in
    /// [`Self::submit`]); `true` = angenommen (auch „unbekannter Befehl": der wurde behandelt).
    async fn handle_command(&mut self, cmd: &str) -> bool {
        let mut it = cmd.splitn(2, ' ');
        let name = it.next().unwrap_or("");
        let arg = it
            .next()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());

        if self.running
            && matches!(
                name,
                "new" | "compact" | "model" | "tree" | "resume" | "reload" | "trust"
            )
        {
            self.notify("läuft noch — bitte warten");
            return false;
        }

        // Neue Builtins auch in BUILTIN_COMMANDS (Modulebene) eintragen — Grundlage der
        // Kollisionswarnung für gleichnamige Prompt-Templates.
        match name {
            "quit" | "exit" | "q" => {
                // Wie Ctrl+C (on_term_event): einen laufenden Turn canceln — sonst hinge
                // run() nach dem Loop-Ende in session.lock().await, bis der Turn (Streaming,
                // Tools) von selbst fertig ist, und Ctrl+C in dem Zustand killte ohne
                // finalize()/fsync.
                if let Some(c) = self.cancel.take() {
                    c.cancel();
                }
                self.should_quit = true;
            }
            "help" | "h" => {
                let mut text = String::from(
                    "Befehle: /new /resume /tree /compact /model [id] /trust /reload /hide /show /quit",
                );
                if !self.prompt_templates.is_empty() {
                    let names: Vec<String> = self
                        .prompt_templates
                        .iter()
                        .map(|(n, _)| format!("/{n}"))
                        .collect();
                    text.push_str(&format!("\nPrompt-Templates: {}", names.join(" ")));
                }
                self.transcript.push(DisplayMsg {
                    kind: Kind::Info,
                    text,
                });
            }
            "new" => match session::new_store() {
                Ok(store) => {
                    let mut g = self.session.lock().await;
                    // Alte Session durabel abschließen (fsync), bevor wir umschalten.
                    let _ = g.finalize().await;
                    g.set_session(store);
                    drop(g);
                    self.transcript.clear();
                    self.notices.clear();
                    self.streaming = None;
                    self.streaming_thinking = None;
                    self.scroll_back = 0;
                    self.notify("neue Session");
                }
                Err(e) => self.notify_error(format!("/new: {e}")),
            },
            "model" => match arg {
                Some(id) => {
                    // Custom-Modelle erben den TATSÄCHLICHEN Session-Provider (nicht das
                    // provider-Tag des aktuellen Modells, das nach einem Cross-Provider-Start
                    // falsch wäre) — korrekte Compaction-Schwelle statt pauschal anthropic/200k.
                    // `--provider local` meldet sich als "openai"; custom_model behandelt beide
                    // gleich (128k). Anzeige über model_label statt "(custom)"-display_name.
                    let mut g = self.session.lock().await;
                    let provider = g.provider_name().to_string();
                    let model = models::find_model(&id)
                        .unwrap_or_else(|| crate::custom_model(id, &provider));
                    let label = crate::model_label(&model).to_string();
                    g.set_model(model);
                    drop(g);
                    self.notify(format!("Modell: {label}"));
                }
                None => {
                    let ids: Vec<String> =
                        models::builtin_models().into_iter().map(|m| m.id).collect();
                    self.notify(format!("Modelle: {}", ids.join(", ")));
                }
            },
            "compact" => self.start_compact(),
            "tree" => {
                // Guard vor den notify-Aufrufen (&mut self) freigeben.
                let g = self.session.lock().await;
                let built = g.session().map(build_tree);
                drop(g);
                match built {
                    Some((lines, sel)) => {
                        if lines.is_empty() {
                            self.notify("Baum ist leer");
                        } else {
                            self.tree_lines = lines;
                            self.tree_sel = sel;
                            self.mode = Mode::Tree;
                        }
                    }
                    None => self.notify("keine persistente Session"),
                }
            }
            "resume" => match session::list_sessions() {
                Ok(list) if !list.is_empty() => {
                    self.resume_list = list;
                    self.resume_sel = 0;
                    self.mode = Mode::Resume;
                }
                Ok(_) => self.notify("keine gespeicherten Sessions"),
                Err(e) => self.notify_error(format!("/resume: {e}")),
            },
            "trust" => match session::trust_current_project() {
                Ok(()) => {
                    // Genau EINE Meldung aus der Rückgabe bauen — self.status zurückzulesen
                    // erzeugte bei /hide zwei fast identische Transcript-Zeilen. Bei None steht
                    // der Reload-Fehler bereits rot da (das Projekt ist trotzdem vertraut).
                    if let Some(summary) = self.reload_resources().await {
                        self.notify(format!("Projekt vertraut · {summary}"));
                    }
                }
                Err(e) => self.notify_error(format!("/trust: {e}")),
            },
            "reload" => {
                if let Some(summary) = self.reload_resources().await {
                    self.notify(summary);
                }
            }
            // Bewusst ohne Session-Lock: der Prompt-Task hält den Mutex für die gesamte
            // Turn-Dauer — ein lock().await hier fröre die Event-Loop bis Turn-Ende ein
            // (kein Rendern, kein Esc). self.status wird ohnehin durchgängig gepflegt.
            "hide" => self.show_status = false,
            "show" => self.show_status = true,
            other => {
                // Prompt-Template als Slash-Command?
                let content = self
                    .prompt_templates
                    .iter()
                    .find(|(n, _)| n == other)
                    .map(|(_, c)| c.clone());
                match content {
                    Some(content) => {
                        if self.running {
                            self.notify("läuft noch — bitte warten");
                            return false;
                        }
                        let expanded = match arg {
                            Some(a) => format!("{content} {a}"),
                            None => content,
                        };
                        self.transcript.push(DisplayMsg {
                            kind: Kind::User,
                            text: expanded.clone(),
                        });
                        self.scroll_back = 0;
                        self.start_prompt(expanded);
                    }
                    None => self.notify(format!("unbekannter Befehl: /{other}")),
                }
            }
        }
        true
    }

    /// Lädt Resources (Skills → System-Prompt, Prompt-Templates) und Hooks neu von Platte.
    /// `Some(summary)` bei Erfolg — die Meldung setzt der AUFRUFER ab (`/trust` präfixt sie),
    /// statt sie aus `self.status` zurückzulesen; `None`, wenn ein Fehler bereits via
    /// [`Self::notify_error`] gemeldet wurde.
    async fn reload_resources(&mut self) -> Option<String> {
        let trusted = session::is_project_trusted().unwrap_or(false);
        let roots = match session::resource_roots(trusted) {
            Ok(r) => r,
            Err(e) => {
                self.notify_error(format!("/reload: {e}"));
                return None;
            }
        };
        let res = ResourceSet::load(&roots);
        let nskills = res.skills.len();
        let system = format!("{}{}", self.base_prompt, res.system_prompt_addition());

        // Hook-Fehler NICHT verschlucken (vorher eine `.ok()`-Kette): ein Rhai-Syntaxfehler in
        // EINEM Skript würde sonst via set_hooks(None) alle Hooks — auch intakte Policy-Guards
        // — kommentarlos deaktivieren. Konsistent zum Startup (main.rs bailt hart), für die
        // laufende TUI abgeschwächt: Fehler melden, bestehende Hooks unangetastet lassen.
        let hooks_res: std::result::Result<Option<Box<dyn HookHost>>, String> =
            match session::hook_dirs(trusted) {
                Ok(dirs) => match RhaiHookHost::from_dirs(&dirs) {
                    Ok(h) if h.is_empty() => Ok(None),
                    Ok(h) => Ok(Some(Box::new(h) as Box<dyn HookHost>)),
                    Err(e) => Err(e.to_string()),
                },
                Err(e) => Err(e.to_string()),
            };
        let (nhooks, hook_err) = match &hooks_res {
            Ok(Some(_)) => (1, None),
            Ok(None) => (0, None),
            Err(e) => (0, Some(e.clone())),
        };

        {
            let mut g = self.session.lock().await;
            g.set_system_prompt(system);
            if let Ok(hooks) = hooks_res {
                g.set_hooks(hooks);
            }
        }
        self.prompt_templates = res
            .prompts
            .into_iter()
            .map(|p| (p.name, p.content))
            .collect();
        if let Some(e) = hook_err {
            self.notify_error(format!(
                "/reload: Hooks fehlgeschlagen: {e} — bestehende Hooks bleiben aktiv; \
                 Skills/Templates wurden aktualisiert"
            ));
            return None;
        }
        let mut summary = format!(
            "neu geladen · {nskills} Skills · {} Templates · {} Hook-Quelle(n)",
            self.prompt_templates.len(),
            nhooks
        );
        let shadowed = template_collisions(&self.prompt_templates);
        if !shadowed.is_empty() {
            summary.push_str(&format!(
                " · Achtung: {} von Builtins verschattet",
                slash_list(&shadowed)
            ));
        }
        Some(summary)
    }

    fn start_compact(&mut self) {
        // Neue Nutzeraktion: bisheriges Notice-Feedback hat seinen Zweck erfüllt.
        self.notices.clear();
        self.running = true;
        self.status = "komprimiere …".into();
        let cancel = CancellationToken::new();
        self.cancel = Some(cancel);
        let sess = self.session.clone();
        let tx = self.tx.clone();
        tokio::spawn(async move {
            let mut guard = DoneOnDrop::new(tx.clone());
            let res = {
                let mut g = sess.lock().await;
                g.compact(None).await
            };
            guard.disarm();
            let _ = tx.send(UiMsg::Done(res.err().map(|e| e.to_string())));
        });
    }

    async fn on_tree_key(&mut self, code: KeyCode) {
        match code {
            KeyCode::Esc | KeyCode::Char('q') => self.mode = Mode::Chat,
            KeyCode::Up | KeyCode::Char('k') => self.tree_sel = self.tree_sel.saturating_sub(1),
            KeyCode::Down | KeyCode::Char('j') => {
                self.tree_sel = (self.tree_sel + 1).min(self.tree_lines.len().saturating_sub(1))
            }
            KeyCode::Enter => {
                if let Some(line) = self.tree_lines.get(self.tree_sel) {
                    let id = line.id.clone();
                    let mut g = self.session.lock().await;
                    let err = g.session_mut().and_then(|s| s.branch(&id).err());
                    g.reload_from_session();
                    let t = transcript_from_messages(g.messages(), self.show_thinking);
                    drop(g);
                    self.transcript = t;
                    self.notices.clear();
                    self.streaming = None;
                    self.streaming_thinking = None;
                    self.mode = Mode::Chat;
                    self.scroll_back = 0;
                    match err {
                        Some(e) => self.notify_error(format!("branch: {e}")),
                        None => self.notify("verzweigt — neuer Ast"),
                    }
                }
            }
            _ => {}
        }
    }

    async fn on_resume_key(&mut self, code: KeyCode) {
        match code {
            KeyCode::Esc | KeyCode::Char('q') => self.mode = Mode::Chat,
            KeyCode::Up | KeyCode::Char('k') => self.resume_sel = self.resume_sel.saturating_sub(1),
            KeyCode::Down | KeyCode::Char('j') => {
                self.resume_sel =
                    (self.resume_sel + 1).min(self.resume_list.len().saturating_sub(1))
            }
            KeyCode::Enter => {
                if let Some(info) = self.resume_list.get(self.resume_sel).cloned() {
                    match sepp_session::JsonlSessionStore::open(&info.path) {
                        Ok(store) => {
                            let mut g = self.session.lock().await;
                            // Alte Session abschließen, bevor wir auf die gewählte umschalten.
                            let _ = g.finalize().await;
                            g.set_session(Box::new(store));
                            let t = transcript_from_messages(g.messages(), self.show_thinking);
                            drop(g);
                            self.transcript = t;
                            self.notices.clear();
                            self.streaming = None;
                            self.streaming_thinking = None;
                            self.scroll_back = 0;
                            self.mode = Mode::Chat;
                            self.notify(format!("Session {} geladen", short_id(&info.id)));
                        }
                        Err(e) => self.notify_error(format!("öffnen: {e}")),
                    }
                }
            }
            _ => {}
        }
    }

    // ---- Agent-Events ----------------------------------------------------

    async fn on_ui_msg(&mut self, msg: UiMsg) {
        match msg {
            UiMsg::Agent(ev) => self.on_agent_event(ev),
            UiMsg::Done(err) => {
                self.finalize_streaming();
                self.running = false;
                self.cancel = None;
                // Transcript mit der kanonischen Conversation abgleichen.
                self.rebuild_transcript().await;
                if let Some(e) = err {
                    self.transcript.push(DisplayMsg {
                        kind: Kind::Error,
                        text: format!("[Fehler] {e}"),
                    });
                    self.status = "Fehler".into();
                }
                self.scroll_back = 0;
            }
        }
    }

    fn on_agent_event(&mut self, ev: AgentEvent) {
        match ev {
            AgentEvent::TextDelta(s) => {
                self.streaming.get_or_insert_with(String::new).push_str(&s);
                self.scroll_back = 0;
            }
            AgentEvent::ThinkingDelta(s) if self.show_thinking => {
                self.streaming_thinking
                    .get_or_insert_with(String::new)
                    .push_str(&s);
                self.scroll_back = 0;
            }
            AgentEvent::ToolStart { name, .. } => {
                self.finalize_streaming();
                self.transcript.push(DisplayMsg {
                    kind: Kind::Info,
                    text: format!("· {name} …"),
                });
                self.scroll_back = 0;
            }
            AgentEvent::ToolEnd { is_error, .. } if is_error => {
                self.transcript.push(DisplayMsg {
                    kind: Kind::Info,
                    text: "· (Tool-Fehler)".into(),
                });
            }
            AgentEvent::TurnEnd => self.finalize_streaming(),
            // Bewusst ins Transcript statt in die Notices: auf AgentEvent::Error folgt im
            // Agent-Loop immer ein Err → UiMsg::Done(Some(e)), und der Done-Handler pusht die
            // [Fehler]-Zeile NACH dem rebuild_transcript erneut — eine Notice ergäbe eine
            // Doppelanzeige. Diese Live-Zeile hier überbrückt nur bis zum Done.
            AgentEvent::Error(e) => self.transcript.push(DisplayMsg {
                kind: Kind::Error,
                text: format!("[Fehler] {e}"),
            }),
            _ => {}
        }
    }

    fn finalize_streaming(&mut self) {
        // Reihenfolge wie in der Nachricht: Thinking VOR Text.
        if let Some(t) = self.streaming_thinking.take() {
            if !t.trim().is_empty() {
                self.transcript.push(DisplayMsg {
                    kind: Kind::Thinking,
                    text: t,
                });
            }
        }
        if let Some(s) = self.streaming.take() {
            if !s.trim().is_empty() {
                self.transcript.push(DisplayMsg {
                    kind: Kind::Assistant,
                    text: s,
                });
            }
        }
    }

    // ---- Rendering -------------------------------------------------------

    fn render(&mut self, f: &mut Frame) {
        let area = f.area();
        match self.mode {
            Mode::Chat => self.render_chat(f, area),
            Mode::Tree => {
                let items: Vec<String> =
                    self.tree_lines.iter().map(|l| l.display.clone()).collect();
                render_list(
                    f,
                    area,
                    "Baum — ↑/↓ wählen · Enter: verzweigen · Esc: zurück",
                    &items,
                    self.tree_sel,
                );
            }
            Mode::Resume => {
                let items: Vec<String> = self
                    .resume_list
                    .iter()
                    .map(|s| format!("{}  · {} Einträge", short_id(&s.id), s.entry_count))
                    .collect();
                render_list(
                    f,
                    area,
                    "Sessions — ↑/↓ wählen · Enter: laden · Esc: zurück",
                    &items,
                    self.resume_sel,
                );
            }
        }
    }

    fn render_chat(&mut self, f: &mut Frame, area: Rect) {
        let chunks = Layout::vertical(chat_constraints(self.show_status)).split(area);

        let chat_area = chunks[0];
        let inner_w = chat_area.width.saturating_sub(2).max(1) as usize;
        let view_h = chat_area.height.saturating_sub(2);

        let mut lines: Vec<Line> = Vec::new();
        for m in &self.transcript {
            let (text, style) = styled(m);
            for row in wrap(&text, inner_w) {
                lines.push(Line::styled(row, style));
            }
        }
        // Live streamendes Reasoning gedimmt VOR dem streamenden Antworttext.
        if let Some(t) = &self.streaming_thinking {
            let style = Style::default()
                .fg(Color::DarkGray)
                .add_modifier(Modifier::DIM);
            for row in wrap(t, inner_w) {
                lines.push(Line::styled(row, style));
            }
        }
        if let Some(s) = &self.streaming {
            for row in wrap(s, inner_w) {
                lines.push(Line::styled(row, Style::default()));
            }
        }
        // Notices (Feedback bei versteckter Statuszeile, Start-Hinweise) ganz unten — VOR der
        // total-Berechnung, damit der Scroll-Anker sie einschließt.
        for m in &self.notices {
            let (text, style) = styled(m);
            for row in wrap(&text, inner_w) {
                lines.push(Line::styled(row, style));
            }
        }

        // Scroll-Arithmetik in usize (kein u16-Truncation bei sehr langen Transkripten);
        // erst der finale, geclampte Offset wird auf u16 gecastet (ratatui-Scroll ist u16).
        let total = lines.len();
        let view = view_h as usize;
        let max_scroll = total.saturating_sub(view).min(u16::MAX as usize) as u16;
        self.scroll_back = self.scroll_back.min(max_scroll);
        let scroll = max_scroll - self.scroll_back;

        let chat = Paragraph::new(lines)
            .block(Block::default().borders(Borders::ALL).title("sepp mini"))
            .scroll((scroll, 0));
        f.render_widget(chat, chat_area);

        // Die Statuszeile hat via chat_constraints Höhe 0, wenn versteckt — nur bei
        // Sichtbarkeit rendern (spart zudem den String-Clone pro Frame).
        if self.show_status {
            let status = Paragraph::new(Line::styled(
                self.status.clone(),
                Style::default().fg(Color::Yellow),
            ));
            f.render_widget(status, chunks[1]);
        }
        let input_area = chunks[2];

        let input = Paragraph::new(format!("> {}", self.input)).block(
            Block::default()
                .borders(Borders::ALL)
                .title("Eingabe · /help"),
        );
        f.render_widget(input, input_area);

        f.set_cursor_position((
            cursor_x(input_area, self.input.chars().count()),
            input_area.y + 1,
        ));
    }
}

fn styled(m: &DisplayMsg) -> (String, Style) {
    match m.kind {
        Kind::User => (
            format!("» {}", m.text),
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
        Kind::Assistant => (m.text.clone(), Style::default()),
        Kind::Thinking => (
            m.text.clone(),
            Style::default()
                .fg(Color::DarkGray)
                .add_modifier(Modifier::DIM),
        ),
        Kind::Info => (m.text.clone(), Style::default().fg(Color::DarkGray)),
        Kind::Error => (m.text.clone(), Style::default().fg(Color::Red)),
    }
}

fn render_list(f: &mut Frame, area: Rect, title: &str, items: &[String], selected: usize) {
    let block = Block::default()
        .borders(Borders::ALL)
        .title(title.to_string());
    let inner = block.inner(area);
    f.render_widget(block, area);

    let h = inner.height.max(1) as usize;
    let off = selected.saturating_sub(h.saturating_sub(1));
    let mut lines: Vec<Line> = Vec::new();
    for (i, it) in items.iter().enumerate().skip(off).take(h) {
        let style = if i == selected {
            Style::default().fg(Color::Black).bg(Color::Cyan)
        } else {
            Style::default()
        };
        lines.push(Line::styled(it.clone(), style));
    }
    f.render_widget(Paragraph::new(lines), inner);
}

fn idle_status(g: &AgentSession) -> String {
    // Bewusst ohne Token-Zähler — nur Modell.
    format!("bereit · {} · /help", crate::model_label(g.model()))
}

fn short_id(id: &str) -> String {
    id.chars().take(8).collect()
}

/// Builtin-Slash-Commands inkl. Aliase — MUSS mit dem `match name` in `handle_command`
/// synchron bleiben. Grundlage der Kollisionswarnung beim Laden der Prompt-Templates.
const BUILTIN_COMMANDS: &[&str] = &[
    "quit", "exit", "q", "help", "h", "new", "model", "compact", "tree", "resume", "trust",
    "reload", "hide", "show",
];

/// Namen der Templates, die ein Builtin-Kommando verschatten würden — der Builtin gewinnt
/// im Dispatch, solche Templates sind per Slash unerreichbar.
fn template_collisions(templates: &[(String, String)]) -> Vec<String> {
    templates
        .iter()
        .filter(|(n, _)| BUILTIN_COMMANDS.contains(&n.as_str()))
        .map(|(n, _)| n.clone())
        .collect()
}

/// Namen als `/name`-Liste für Meldungen (`["a","b"]` → `"/a /b"`).
fn slash_list(names: &[String]) -> String {
    names
        .iter()
        .map(|n| format!("/{n}"))
        .collect::<Vec<_>>()
        .join(" ")
}

/// Layout-Zonen des Chat-Screens: Chat, Statuszeile (Höhe 0, wenn via `/hide` versteckt),
/// Eingabe — `chunks[2]` ist damit IMMER das Eingabefeld, unabhängig von `show_status`.
fn chat_constraints(show_status: bool) -> [Constraint; 3] {
    [
        Constraint::Min(1),
        Constraint::Length(u16::from(show_status)),
        Constraint::Length(3),
    ]
}

/// Cursor-Spalte: in usize gerechnet (kein u16-Overflow-Panic im Debug-Build bei sehr langen
/// Eingaben), final auf die rechte Innenkante geclampt — Muster wie die Scroll-Arithmetik in
/// `render_chat`. `+ 3` = Rahmen + `"> "`-Präfix.
fn cursor_x(area: Rect, input_chars: usize) -> u16 {
    let right = (area.x as usize + area.width.saturating_sub(1) as usize).min(u16::MAX as usize);
    (area.x as usize + 3 + input_chars).min(right) as u16
}

fn transcript_from_messages(msgs: &[Message], show_thinking: bool) -> Vec<DisplayMsg> {
    let mut out = Vec::new();
    for m in msgs {
        match m.role {
            Role::User => {
                let mut text = String::new();
                let mut tool_result = false;
                for b in &m.content {
                    match b {
                        ContentBlock::Text { text: t } => push_line(&mut text, t),
                        ContentBlock::ToolResult { .. } => tool_result = true,
                        _ => {}
                    }
                }
                if !text.is_empty() {
                    out.push(DisplayMsg {
                        kind: Kind::User,
                        text,
                    });
                } else if tool_result {
                    out.push(DisplayMsg {
                        kind: Kind::Info,
                        text: "· Tool-Ergebnisse".into(),
                    });
                }
            }
            Role::Assistant => {
                let mut text = String::new();
                for b in &m.content {
                    match b {
                        ContentBlock::Thinking { text: t, .. }
                            if show_thinking && !t.trim().is_empty() =>
                        {
                            out.push(DisplayMsg {
                                kind: Kind::Thinking,
                                text: t.clone(),
                            });
                        }
                        ContentBlock::Text { text: t } => push_line(&mut text, t),
                        ContentBlock::ToolUse { name, .. } => out.push(DisplayMsg {
                            kind: Kind::Info,
                            text: format!("· ruft {name}"),
                        }),
                        _ => {}
                    }
                }
                if !text.is_empty() {
                    out.push(DisplayMsg {
                        kind: Kind::Assistant,
                        text,
                    });
                }
            }
            _ => {}
        }
    }
    out
}

fn push_line(buf: &mut String, t: &str) {
    if !buf.is_empty() {
        buf.push('\n');
    }
    buf.push_str(t);
}

fn entry_snippet(payload: &sepp_session::EntryPayload) -> String {
    match payload {
        sepp_session::EntryPayload::Message { message } => {
            let role = match message.role {
                Role::User => "user",
                Role::Assistant => "assistant",
                Role::Tool => "tool",
                Role::System => "system",
            };
            let mut text = String::new();
            for b in &message.content {
                match b {
                    ContentBlock::Text { text: t } => {
                        text.push_str(t);
                        break;
                    }
                    ContentBlock::ToolUse { name, .. } => {
                        text = format!("→ {name}");
                        break;
                    }
                    ContentBlock::ToolResult { .. } => {
                        text = "(Tool-Ergebnis)".into();
                        break;
                    }
                    _ => {}
                }
            }
            let one = text.replace('\n', " ");
            let snippet: String = one.chars().take(48).collect();
            format!("{role}: {snippet}")
        }
        sepp_session::EntryPayload::Compaction { .. } => "[Zusammenfassung]".into(),
        sepp_session::EntryPayload::Custom { kind, .. } => format!("[custom: {kind}]"),
    }
}

fn build_tree(store: &dyn SessionStore) -> (Vec<TreeLine>, usize) {
    use std::collections::HashMap;
    let entries = store.entries();
    let leaf = store.leaf().cloned();

    let mut children: HashMap<Option<String>, Vec<usize>> = HashMap::new();
    for (i, e) in entries.iter().enumerate() {
        children.entry(e.parent_id.clone()).or_default().push(i);
    }

    let mut lines = Vec::new();
    let mut stack: Vec<(usize, usize)> = Vec::new();
    if let Some(roots) = children.get(&None) {
        for &r in roots.iter().rev() {
            stack.push((r, 0));
        }
    }
    while let Some((idx, depth)) = stack.pop() {
        let e = &entries[idx];
        let indent = "  ".repeat(depth);
        let label = e
            .label
            .as_ref()
            .map(|l| format!(" [{l}]"))
            .unwrap_or_default();
        let marker = if leaf.as_ref() == Some(&e.id) {
            "  ←"
        } else {
            ""
        };
        lines.push(TreeLine {
            id: e.id.clone(),
            display: format!("{indent}{}{label}{marker}", entry_snippet(&e.payload)),
        });
        if let Some(ch) = children.get(&Some(e.id.clone())) {
            for &c in ch.iter().rev() {
                stack.push((c, depth + 1));
            }
        }
    }

    let sel = lines
        .iter()
        .position(|l| Some(&l.id) == leaf.as_ref())
        .unwrap_or(0);
    (lines, sel)
}

/// Greedy-Wortumbruch auf `width` Zeichen (erhält leere Zeilen; überlange Wörter werden
/// vom Paragraph beschnitten).
fn wrap(s: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let mut rows = Vec::new();
    for raw in s.split('\n') {
        let mut cur = String::new();
        let mut cur_w = 0usize;
        for word in raw.split_whitespace() {
            let wl = word.chars().count();
            if cur_w == 0 {
                cur.push_str(word);
                cur_w = wl;
            } else if cur_w + 1 + wl <= width {
                cur.push(' ');
                cur.push_str(word);
                cur_w += 1 + wl;
            } else {
                rows.push(std::mem::take(&mut cur));
                cur.push_str(word);
                cur_w = wl;
            }
        }
        rows.push(cur);
    }
    rows
}

#[cfg(test)]
mod tests {
    use super::*;

    use futures::stream::{self, BoxStream};
    use sepp_provider::{CompletionRequest, Provider, StreamEvent};

    /// Provider-Attrappe für App-Tests: leerer Stream, kein Netz, kein Key.
    struct FakeProvider;

    #[async_trait::async_trait]
    impl Provider for FakeProvider {
        fn name(&self) -> &str {
            "fake"
        }
        async fn stream<'a>(
            &'a self,
            _req: CompletionRequest<'a>,
            _cancel: CancellationToken,
        ) -> sepp_core::Result<BoxStream<'a, StreamEvent>> {
            Ok(Box::pin(stream::iter(Vec::new())))
        }
    }

    /// App mit FakeProvider-Session — für Tests der Command-/Notify-Logik ohne Terminal.
    fn test_app() -> App {
        let session = sepp_agent::AgentSession::builder()
            .provider(Arc::new(FakeProvider))
            .model(crate::custom_model("test".into(), "fake"))
            .build()
            .expect("test session");
        let (tx, _rx) = mpsc::unbounded_channel();
        App::new(
            Arc::new(Mutex::new(session)),
            tx,
            Vec::new(),
            String::new(),
            false,
        )
    }

    #[tokio::test]
    async fn quit_during_turn_cancels_and_quits() {
        // /quit während eines laufenden Turns muss den Turn canceln (wie Ctrl+C) — sonst
        // hängt run() nach dem Loop-Ende am Session-Mutex, bis der Turn von selbst endet.
        let mut app = test_app();
        let token = CancellationToken::new();
        app.cancel = Some(token.clone());
        app.running = true;
        app.handle_command("quit").await;
        assert!(token.is_cancelled());
        assert!(app.should_quit);
    }

    #[tokio::test]
    async fn notices_survive_transcript_rebuild() {
        // Bei /hide ist die Notice die einzige sichtbare Kopie des Feedbacks — sie muss den
        // Transcript-Neuaufbau am Turn-Ende (Done → rebuild_transcript) überleben.
        let mut app = test_app();
        app.show_status = false;
        app.notify_error("/new: kaputt");
        app.rebuild_transcript().await;
        assert!(app
            .notices
            .iter()
            .any(|m| m.kind == Kind::Error && m.text.contains("/new: kaputt")));
        // Das Transcript selbst ist frisch aus den (leeren) Session-Messages gebaut.
        assert!(app.transcript.is_empty());
    }

    #[tokio::test]
    async fn notify_hidden_resets_scroll_visible_keeps_it() {
        let mut app = test_app();
        // Versteckte Statuszeile: die Notice ist nur am Verlaufs-Ende sichtbar → Scroll-Reset.
        app.show_status = false;
        app.scroll_back = 5;
        app.notify("läuft noch — bitte warten");
        assert_eq!(app.scroll_back, 0);
        // Sichtbare Statuszeile: der Chat-Inhalt ändert sich nicht, die Scroll-Position des
        // Nutzers bleibt unangetastet.
        app.show_status = true;
        app.scroll_back = 5;
        app.notify("Modelle: …");
        assert_eq!(app.scroll_back, 5);
    }

    #[tokio::test]
    async fn start_prompt_clears_notices() {
        // Eine neue Nutzeraktion beendet die Lebenszeit des bisherigen Feedbacks.
        let mut app = test_app();
        app.show_status = false;
        app.notify("alter Hinweis");
        assert!(!app.notices.is_empty());
        app.start_prompt("hallo".into());
        assert!(app.notices.is_empty());
    }

    #[tokio::test]
    async fn rejected_command_keeps_input() {
        // Ein wegen laufendem Turn abgewiesener Befehl darf die Eingabe nicht verwerfen —
        // Parität zum Eingabe-Erhalt für normale Prompts.
        let mut app = test_app();
        app.running = true;
        app.input = "/compact".into();
        app.submit().await;
        assert_eq!(app.input, "/compact");
        assert_eq!(app.status, "läuft noch — bitte warten");
    }

    #[tokio::test]
    async fn rejected_template_keeps_input() {
        let mut app = test_app();
        app.prompt_templates = vec![("review".into(), "Prüfe:".into())];
        app.running = true;
        app.input = "/review langer mühsam getippter Kontext".into();
        app.submit().await;
        assert_eq!(app.input, "/review langer mühsam getippter Kontext");
    }

    #[tokio::test]
    async fn accepted_command_clears_input() {
        let mut app = test_app();
        app.input = "/help".into();
        app.submit().await;
        assert!(app.input.is_empty());
        assert!(app
            .transcript
            .iter()
            .any(|m| m.kind == Kind::Info && m.text.contains("Befehle:")));
    }

    #[test]
    fn wrap_breaks_on_width() {
        let rows = wrap("aaa bbb ccc", 7);
        assert_eq!(rows, vec!["aaa bbb".to_string(), "ccc".to_string()]);
    }

    #[test]
    fn wrap_preserves_blank_lines() {
        let rows = wrap("a\n\nb", 80);
        assert_eq!(rows, vec!["a".to_string(), "".to_string(), "b".to_string()]);
    }

    #[test]
    fn transcript_maps_roles() {
        let msgs = vec![
            Message::user_text("hi"),
            Message::assistant(vec![ContentBlock::text("hallo")]),
        ];
        let t = transcript_from_messages(&msgs, true);
        assert_eq!(t.len(), 2);
        assert_eq!(t[0].kind, Kind::User);
        assert_eq!(t[1].kind, Kind::Assistant);
    }

    #[test]
    fn transcript_includes_thinking_when_enabled() {
        let msgs = vec![Message::assistant(vec![
            ContentBlock::Thinking {
                text: "kurz nachgedacht".into(),
                signature: None,
            },
            ContentBlock::text("Antwort"),
        ])];
        // show_thinking=true → Thinking VOR Text.
        let on = transcript_from_messages(&msgs, true);
        assert_eq!(on.len(), 2);
        assert_eq!(on[0].kind, Kind::Thinking);
        assert_eq!(on[1].kind, Kind::Assistant);
        // show_thinking=false → nur die Antwort.
        let off = transcript_from_messages(&msgs, false);
        assert_eq!(off.len(), 1);
        assert_eq!(off[0].kind, Kind::Assistant);
    }

    #[test]
    fn chat_constraints_toggle_status_row() {
        // chunks[2] ist immer das Eingabefeld; /hide setzt nur die Statuszeile auf Höhe 0.
        assert_eq!(chat_constraints(true)[1], Constraint::Length(1));
        assert_eq!(chat_constraints(false)[1], Constraint::Length(0));
        assert_eq!(chat_constraints(true)[2], Constraint::Length(3));
        assert_eq!(chat_constraints(false)[2], Constraint::Length(3));
    }

    #[test]
    fn cursor_x_clamps_without_overflow() {
        let area = Rect::new(0, 20, 80, 3);
        // Normalfall: x + 3 + Eingabelänge.
        assert_eq!(cursor_x(area, 5), 8);
        // ~70k Zeichen (Paste): kein u16-Overflow-Panic, Clamp auf die rechte Innenkante.
        assert_eq!(cursor_x(area, 70_000), 79);
    }

    #[test]
    fn template_collisions_finds_builtin_shadowing() {
        let templates = vec![
            ("model".to_string(), "…".to_string()),
            ("review".to_string(), "…".to_string()),
            ("hide".to_string(), "…".to_string()),
        ];
        assert_eq!(template_collisions(&templates), ["model", "hide"]);
        assert!(template_collisions(&[("review".to_string(), "…".to_string())]).is_empty());
    }
}
