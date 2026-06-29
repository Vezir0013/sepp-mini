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
use sepp_core::{ContentBlock, Message, Model, Role};
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
    tx: mpsc::UnboundedSender<UiMsg>,
}

/// Startet die TUI und blockiert, bis der Nutzer beendet.
pub async fn run(
    agent: AgentSession,
    prompt_templates: Vec<(String, String)>,
    base_prompt: String,
    show_thinking: bool,
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
            tx,
        }
    }

    async fn rebuild_transcript(&mut self) {
        let g = self.session.lock().await;
        self.transcript = transcript_from_messages(g.messages(), self.show_thinking);
        self.status = idle_status(&g);
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
        self.input.clear();
        if let Some(cmd) = text.strip_prefix('/') {
            self.handle_command(cmd).await;
            return;
        }
        if self.running {
            self.status = "läuft noch — bitte warten".into();
            return;
        }
        self.transcript.push(DisplayMsg {
            kind: Kind::User,
            text: text.clone(),
        });
        self.scroll_back = 0;
        self.start_prompt(text);
    }

    fn start_prompt(&mut self, text: String) {
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

    async fn handle_command(&mut self, cmd: &str) {
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
            self.status = "läuft noch — bitte warten".into();
            return;
        }

        match name {
            "quit" | "exit" | "q" => self.should_quit = true,
            "help" | "h" => {
                let mut text = String::from(
                    "Befehle: /new /resume /tree /compact /model [id] /trust /reload /quit",
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
                    self.streaming = None;
                    self.streaming_thinking = None;
                    self.scroll_back = 0;
                    self.status = "neue Session".into();
                }
                Err(e) => self.status = format!("/new: {e}"),
            },
            "model" => match arg {
                Some(id) => {
                    let model = models::find_model(&id).unwrap_or_else(|| custom_model(&id));
                    let name = model.display_name.clone();
                    self.session.lock().await.set_model(model);
                    self.status = format!("Modell: {name}");
                }
                None => {
                    let ids: Vec<String> =
                        models::builtin_models().into_iter().map(|m| m.id).collect();
                    self.status = format!("Modelle: {}", ids.join(", "));
                }
            },
            "compact" => self.start_compact(),
            "tree" => {
                let g = self.session.lock().await;
                match g.session() {
                    Some(store) => {
                        let (lines, sel) = build_tree(store);
                        drop(g);
                        if lines.is_empty() {
                            self.status = "Baum ist leer".into();
                        } else {
                            self.tree_lines = lines;
                            self.tree_sel = sel;
                            self.mode = Mode::Tree;
                        }
                    }
                    None => self.status = "keine persistente Session".into(),
                }
            }
            "resume" => match session::list_sessions() {
                Ok(list) if !list.is_empty() => {
                    self.resume_list = list;
                    self.resume_sel = 0;
                    self.mode = Mode::Resume;
                }
                Ok(_) => self.status = "keine gespeicherten Sessions".into(),
                Err(e) => self.status = format!("/resume: {e}"),
            },
            "trust" => match session::trust_current_project() {
                Ok(()) => {
                    self.reload_resources().await;
                    self.status = format!("Projekt vertraut · {}", self.status);
                }
                Err(e) => self.status = format!("/trust: {e}"),
            },
            "reload" => self.reload_resources().await,
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
                            self.status = "läuft noch — bitte warten".into();
                            return;
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
                    None => self.status = format!("unbekannter Befehl: /{other}"),
                }
            }
        }
    }

    /// Lädt Resources (Skills → System-Prompt, Prompt-Templates) und Hooks neu von Platte.
    async fn reload_resources(&mut self) {
        let trusted = session::is_project_trusted().unwrap_or(false);
        let roots = match session::resource_roots(trusted) {
            Ok(r) => r,
            Err(e) => {
                self.status = format!("/reload: {e}");
                return;
            }
        };
        let res = ResourceSet::load(&roots);
        let nskills = res.skills.len();
        let system = format!("{}{}", self.base_prompt, res.system_prompt_addition());

        let hooks: Option<Box<dyn HookHost>> = session::hook_dirs(trusted)
            .ok()
            .and_then(|dirs| RhaiHookHost::from_dirs(&dirs).ok())
            .filter(|h| !h.is_empty())
            .map(|h| Box::new(h) as Box<dyn HookHost>);
        let nhooks = hooks.as_ref().map(|_| 1).unwrap_or(0);

        {
            let mut g = self.session.lock().await;
            g.set_system_prompt(system);
            g.set_hooks(hooks);
        }
        self.prompt_templates = res
            .prompts
            .into_iter()
            .map(|p| (p.name, p.content))
            .collect();
        self.status = format!(
            "neu geladen · {nskills} Skills · {} Templates · {} Hook-Quelle(n)",
            self.prompt_templates.len(),
            nhooks
        );
    }

    fn start_compact(&mut self) {
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
                    self.streaming = None;
                    self.streaming_thinking = None;
                    self.mode = Mode::Chat;
                    self.scroll_back = 0;
                    self.status = match err {
                        Some(e) => format!("branch: {e}"),
                        None => "verzweigt — neuer Ast".into(),
                    };
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
                            self.streaming = None;
                            self.streaming_thinking = None;
                            self.scroll_back = 0;
                            self.mode = Mode::Chat;
                            self.status = format!("Session {} geladen", short_id(&info.id));
                        }
                        Err(e) => self.status = format!("öffnen: {e}"),
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
        let chunks = Layout::vertical([
            Constraint::Min(1),
            Constraint::Length(1),
            Constraint::Length(3),
        ])
        .split(area);

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

        let status = Paragraph::new(Line::styled(
            self.status.clone(),
            Style::default().fg(Color::Yellow),
        ));
        f.render_widget(status, chunks[1]);

        let input = Paragraph::new(format!("> {}", self.input)).block(
            Block::default()
                .borders(Borders::ALL)
                .title("Eingabe · /help"),
        );
        f.render_widget(input, chunks[2]);

        let cx = chunks[2].x + 3 + self.input.chars().count() as u16;
        let cy = chunks[2].y + 1;
        f.set_cursor_position((cx.min(chunks[2].x + chunks[2].width.saturating_sub(1)), cy));
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

fn custom_model(id: &str) -> Model {
    Model {
        id: id.to_string(),
        provider: "anthropic".into(),
        display_name: "(custom)".into(),
        context_window: 200_000,
        max_output_tokens: 8192,
        supports_reasoning: true,
        supports_images: true,
    }
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
}
