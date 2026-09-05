//! `sepp audit [<session>]` — die Spur einer Sitzung für Menschen lesbar machen.
//!
//! Zeigt in der Reihenfolge, in der es passiert ist: Prompts, Antworten, Tool-Aufrufe und
//! ‑Ergebnisse, **Guard-Entscheidungen** (auch die erlaubten) und delegierte Sub-Agenten. Die
//! Kind-Session eines Sub-Agenten wird eingerückt aufgeklappt, sodass eine Delegation nicht mehr
//! im Nichts endet.
//!
//! Der Renderer ([`render_audit`]) ist rein — er bekommt geladene [`SessionView`]s und gibt
//! Text zurück; alles Dateibehaftete liegt in [`run_audit`].
//!
//! **Eine Zuordnung, die man kennen muss:** alle Tools teilen sich einen Guard, und nur die
//! Wurzel-Session zieht dessen Protokoll ab. Entscheidungen, die *während* eines Sub-Agent-Laufs
//! fallen, stehen deshalb in der Wurzel-Spur — unmittelbar vor dem Verweis auf die Kind-Session,
//! nicht in ihr. Das ist bewusst konservativ: eine Aufteilung auf beide Sessions könnte bei
//! parallelen Tool-Aufrufen Entscheidungen der falschen Sitzung zuschlagen.

use std::collections::HashMap;
use std::process::ExitCode;

use anyhow::{anyhow, Result};
use serde_json::json;

use sepp_core::{ContentBlock, Role};
use sepp_session::{Entry, EntryPayload, JsonlSessionStore, SessionInfo, SessionStore};

use crate::session;

/// Wie tief Kind-Sessions aufgeklappt werden (Sub-Agenten können selbst delegieren).
const MAX_DEPTH: usize = 4;
/// Zeichen je Inhaltsvorschau in einer Zeile.
const PREVIEW: usize = 96;
/// Breite der Typ-Spalte, damit die Inhalte untereinander stehen.
const LABEL_WIDTH: usize = 9;

#[derive(Debug, Default, PartialEq, Eq)]
pub struct AuditArgs {
    /// ID-Präfix der Session; ohne Angabe die zuletzt geänderte des Projekts.
    pub select: Option<String>,
    /// Statt Text: ein JSON-Objekt je Eintrag (für `jq`).
    pub json: bool,
    /// Kind-Sessions nicht aufklappen.
    pub no_children: bool,
}

pub fn parse_audit_args(args: &[String]) -> Result<AuditArgs, String> {
    let mut out = AuditArgs::default();
    for a in args {
        match a.as_str() {
            "--json" => out.json = true,
            "--no-children" => out.no_children = true,
            other if other.starts_with('-') => {
                return Err(format!("audit: unbekannte Option: {other}"))
            }
            other if out.select.is_none() => out.select = Some(other.to_string()),
            other => return Err(format!("audit: unerwartetes Argument: {other}")),
        }
    }
    Ok(out)
}

/// Eine geladene Session samt der Kind-Sessions, die daran hängen.
pub struct SessionView {
    pub id: String,
    pub created_at: i64,
    pub cwd: String,
    pub parent: Option<String>,
    pub entries: Vec<Entry>,
    /// Kind-Sessions nach ID, aufgeklappt an ihrem Verweis-Eintrag.
    pub children: HashMap<String, SessionView>,
}

pub fn run_audit(args: AuditArgs) -> ExitCode {
    match audit_text(&args) {
        Ok(text) => {
            print!("{text}");
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("Fehler: {e}");
            ExitCode::FAILURE
        }
    }
}

fn audit_text(args: &AuditArgs) -> Result<String> {
    let all = session::list_sessions()?;
    let info = match &args.select {
        Some(prefix) => session::resolve_session(prefix)?,
        None => all
            .first()
            .cloned()
            .ok_or_else(|| anyhow!("keine Session für dieses Projekt gefunden"))?,
    };
    let depth = if args.no_children { 0 } else { MAX_DEPTH };
    let view = load_view(&info, &all, depth)?;
    Ok(if args.json {
        render_json(&view)
    } else {
        render_audit(&view)
    })
}

/// Lädt eine Session und rekursiv ihre Kind-Sessions (bis `depth` weitere Ebenen).
fn load_view(info: &SessionInfo, all: &[SessionInfo], depth: usize) -> Result<SessionView> {
    let store = JsonlSessionStore::open(&info.path)?;
    let entries = store.entries().to_vec();
    let mut children = HashMap::new();
    if depth > 0 {
        for child in all
            .iter()
            .filter(|c| c.parent_session.as_deref() == Some(info.id.as_str()))
        {
            if let Ok(v) = load_view(child, all, depth - 1) {
                children.insert(child.id.clone(), v);
            }
        }
    }
    Ok(SessionView {
        id: info.id.clone(),
        created_at: info.created_at,
        cwd: info.cwd.clone(),
        parent: info.parent_session.clone(),
        entries,
        children,
    })
}

// ── Rendering ─────────────────────────────────────────────────────────────────────────────

/// Zählwerk für die Fußzeile.
#[derive(Default)]
struct Tally {
    prompts: usize,
    tool_calls: usize,
    denied: usize,
    subagents: usize,
}

pub fn render_audit(view: &SessionView) -> String {
    let mut out = String::new();
    let mut tally = Tally::default();
    render_session(view, 0, &mut out, &mut tally);
    out.push('\n');
    out.push_str(&format!(
        "{} Prompts · {} Tool-Aufrufe · {} verweigert · {} Sub-Agenten\n",
        tally.prompts, tally.tool_calls, tally.denied, tally.subagents
    ));
    out
}

fn render_session(view: &SessionView, depth: usize, out: &mut String, tally: &mut Tally) {
    let pad = "  ".repeat(depth);
    if depth == 0 {
        out.push_str(&format!(
            "Sitzung {}  ·  {}  ·  {} Einträge\n",
            view.id,
            fmt_stamp(view.created_at),
            view.entries.len()
        ));
        if !view.cwd.is_empty() {
            out.push_str(&format!("Verzeichnis {}\n", view.cwd));
        }
        if let Some(p) = &view.parent {
            out.push_str(&format!("Kind-Session von {p}\n"));
        }
        out.push('\n');
    }

    for e in &view.entries {
        for (label, text) in entry_lines(&e.payload, tally) {
            out.push_str(&format!(
                "{pad}{}  {:<width$} {}\n",
                fmt_clock(e.timestamp),
                label,
                text,
                width = LABEL_WIDTH
            ));
        }
        // Verweist der Eintrag auf eine Kind-Session, klappen wir sie hier auf.
        if let EntryPayload::Custom { kind, data } = &e.payload {
            if kind == "subagent" {
                let id = data.get("session").and_then(|v| v.as_str()).unwrap_or("");
                if let Some(child) = view.children.get(id) {
                    render_session(child, depth + 1, out, tally);
                }
            }
        }
    }

    // Kind-Sessions ohne Verweis-Eintrag: der Sub-Agent brach ab, bevor sein Ergebnis zurückkam.
    // Genau die will man im Audit sehen, deshalb hängen sie hinten an.
    let referenced = referenced_children(&view.entries);
    for (id, child) in &view.children {
        if !referenced.contains(&id.as_str()) {
            out.push_str(&format!(
                "{pad}          {:<width$} {} (abgebrochener Lauf, kein Ergebnis)\n",
                "Sub-Agent",
                short(id),
                width = LABEL_WIDTH
            ));
            tally.subagents += 1;
            render_session(child, depth + 1, out, tally);
        }
    }
}

fn referenced_children(entries: &[Entry]) -> Vec<&str> {
    entries
        .iter()
        .filter_map(|e| match &e.payload {
            EntryPayload::Custom { kind, data } if kind == "subagent" => {
                data.get("session").and_then(|v| v.as_str())
            }
            _ => None,
        })
        .collect()
}

/// Die Zeilen eines Eintrags: (Typ-Spalte, Inhalt). Eine Nachricht kann mehrere Zeilen ergeben
/// (Text plus mehrere Tool-Aufrufe im selben Assistant-Turn).
fn entry_lines(payload: &EntryPayload, tally: &mut Tally) -> Vec<(String, String)> {
    match payload {
        EntryPayload::Message { message } => {
            let mut lines = Vec::new();
            for b in &message.content {
                match b {
                    ContentBlock::Text { text } if !text.trim().is_empty() => {
                        let label = match message.role {
                            Role::User => {
                                tally.prompts += 1;
                                "Nutzer"
                            }
                            Role::Assistant => "Modell",
                            _ => "…",
                        };
                        lines.push((label.to_string(), preview(text)));
                    }
                    ContentBlock::ToolUse { name, input, .. } => {
                        tally.tool_calls += 1;
                        lines.push((
                            "Tool →".into(),
                            format!("{name} {}", preview(&input.to_string())),
                        ));
                    }
                    ContentBlock::ToolResult {
                        content, is_error, ..
                    } => {
                        let text = content
                            .iter()
                            .filter_map(|c| match c {
                                ContentBlock::Text { text } => Some(text.as_str()),
                                _ => None,
                            })
                            .collect::<Vec<_>>()
                            .join(" ");
                        let label = if *is_error { "Tool ✗" } else { "Tool ✓" };
                        lines.push((label.into(), preview(&text)));
                    }
                    _ => {}
                }
            }
            lines
        }
        EntryPayload::Compaction { summary, .. } => {
            vec![("Kompakt".into(), preview(summary))]
        }
        EntryPayload::Custom { kind, data } if kind == "guard" => {
            let decision = data.get("decision").and_then(|v| v.as_str()).unwrap_or("?");
            if decision.starts_with("deny") {
                tally.denied += 1;
            }
            let actor = data.get("actor").and_then(|v| v.as_str()).unwrap_or("?");
            let action = data.get("action").and_then(|v| v.as_str()).unwrap_or("?");
            let mut text = format!("{} · {actor} · {action}", decision.to_uppercase());
            if let Some(detail) = data.get("detail").and_then(|v| v.as_str()) {
                text.push_str(&format!(" — {detail}"));
            }
            vec![("Guard".into(), preview(&text))]
        }
        EntryPayload::Custom { kind, data } if kind == "subagent" => {
            tally.subagents += 1;
            let id = data.get("session").and_then(|v| v.as_str()).unwrap_or("?");
            let task = data.get("task").and_then(|v| v.as_str()).unwrap_or("");
            let n = data.get("entries").and_then(|v| v.as_u64()).unwrap_or(0);
            vec![(
                "Sub-Agent".into(),
                preview(&format!("{} · „{task}\" · {n} Einträge", short(id))),
            )]
        }
        EntryPayload::Custom { kind, data } if kind == "aborted" => {
            let reason = data.get("reason").and_then(|v| v.as_str()).unwrap_or("");
            vec![("Abbruch".into(), preview(reason))]
        }
        EntryPayload::Custom { kind, data } => {
            vec![(format!("[{kind}]"), preview(&data.to_string()))]
        }
    }
}

fn render_json(view: &SessionView) -> String {
    let mut out = String::new();
    push_json(view, &mut out);
    out
}

fn push_json(view: &SessionView, out: &mut String) {
    for e in &view.entries {
        let line = json!({
            "session": view.id,
            "parent": view.parent,
            "entry": e,
        });
        out.push_str(&line.to_string());
        out.push('\n');
    }
    for child in view.children.values() {
        push_json(child, out);
    }
}

// ── Kleinkram ─────────────────────────────────────────────────────────────────────────────

/// Einzeilige, gekürzte Vorschau eines beliebigen Inhalts.
fn preview(text: &str) -> String {
    let one: String = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if one.chars().count() <= PREVIEW {
        return one;
    }
    let head: String = one.chars().take(PREVIEW).collect();
    format!("{head}…")
}

fn short(id: &str) -> String {
    id.chars().take(8).collect()
}

/// `YYYY-MM-DD HH:MM:SSZ` aus Millis seit Epoch (UTC, ohne Zeitzonen-Abhängigkeit).
fn fmt_stamp(millis: i64) -> String {
    let (y, m, d, hh, mm, ss) = civil(millis);
    format!("{y:04}-{m:02}-{d:02} {hh:02}:{mm:02}:{ss:02}Z")
}

/// Nur die Uhrzeit, für die Eintragszeilen.
fn fmt_clock(millis: i64) -> String {
    let (_, _, _, hh, mm, ss) = civil(millis);
    format!("{hh:02}:{mm:02}:{ss:02}")
}

/// Millis seit Epoch → (Jahr, Monat, Tag, Stunde, Minute, Sekunde) in UTC.
/// Tage→Datum nach Howard Hinnants `civil_from_days`; keine externe Zeitbibliothek nötig.
fn civil(millis: i64) -> (i64, u32, u32, u32, u32, u32) {
    let secs = millis.div_euclid(1000);
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let (hh, mm, ss) = (rem / 3600, (rem % 3600) / 60, rem % 60);

    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    (y, m as u32, d as u32, hh as u32, mm as u32, ss as u32)
}

#[cfg(test)]
mod tests {
    use super::*;
    use sepp_session::{EntryId, InMemorySessionStore};

    fn args(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn parses_flags_and_prefix() {
        assert_eq!(parse_audit_args(&[]).unwrap(), AuditArgs::default());
        assert_eq!(
            parse_audit_args(&args(&["3f2a", "--json", "--no-children"])).unwrap(),
            AuditArgs {
                select: Some("3f2a".into()),
                json: true,
                no_children: true,
            }
        );
        assert!(parse_audit_args(&args(&["--was"])).is_err());
        assert!(parse_audit_args(&args(&["a", "b"])).is_err());
    }

    #[test]
    fn civil_converts_known_instants() {
        assert_eq!(fmt_stamp(0), "1970-01-01 00:00:00Z");
        // 2026-09-05 12:34:56 UTC
        assert_eq!(fmt_stamp(1_788_611_696_000), "2026-09-05 12:34:56Z");
        assert_eq!(fmt_clock(1_788_611_696_789), "12:34:56");
    }

    #[test]
    fn preview_collapses_and_truncates() {
        assert_eq!(preview("  a\n  b  "), "a b");
        let long = "x".repeat(200);
        let p = preview(&long);
        assert_eq!(p.chars().count(), PREVIEW + 1, "gekürzt plus Ellipse");
        assert!(p.ends_with('…'));
    }

    /// Baut eine Session aus Payloads und gibt Einträge mit aufsteigenden Zeitstempeln zurück.
    fn view(id: &str, parent: Option<&str>, payloads: Vec<EntryPayload>) -> SessionView {
        let mut store = InMemorySessionStore::new();
        let mut ids: Vec<EntryId> = Vec::new();
        for p in payloads {
            ids.push(store.append(p).unwrap());
        }
        let entries = store
            .entries()
            .iter()
            .enumerate()
            .map(|(i, e)| Entry {
                timestamp: 1_788_611_696_000 + i as i64 * 1000,
                ..e.clone()
            })
            .collect();
        SessionView {
            id: id.into(),
            created_at: 1_788_611_696_000,
            cwd: "/projekt".into(),
            parent: parent.map(str::to_string),
            entries,
            children: HashMap::new(),
        }
    }

    fn msg(role: Role, content: Vec<ContentBlock>) -> EntryPayload {
        EntryPayload::Message {
            message: sepp_core::Message {
                role,
                content,
                usage: None,
            },
        }
    }

    fn full_view() -> SessionView {
        let child = view(
            "7c1e0000-0000-0000-0000-000000000000",
            Some("3f2a0000-0000-0000-0000-000000000000"),
            vec![
                msg(Role::User, vec![ContentBlock::text("prüfe die Tests")]),
                msg(Role::Assistant, vec![ContentBlock::text("alles grün")]),
            ],
        );
        let mut root = view(
            "3f2a0000-0000-0000-0000-000000000000",
            None,
            vec![
                msg(Role::User, vec![ContentBlock::text("lies README.md")]),
                msg(
                    Role::Assistant,
                    vec![
                        ContentBlock::text("Ich schaue nach."),
                        ContentBlock::ToolUse {
                            id: "t1".into(),
                            name: "read".into(),
                            input: json!({ "path": "README.md" }),
                        },
                    ],
                ),
                EntryPayload::Custom {
                    kind: "guard".into(),
                    data: json!({
                        "actor": "agent",
                        "action": "fs_read /projekt/README.md",
                        "decision": "allow",
                        "detail": null
                    }),
                },
                msg(
                    Role::User,
                    vec![ContentBlock::ToolResult {
                        tool_use_id: "t1".into(),
                        content: vec![ContentBlock::text("# sepp mini")],
                        is_error: false,
                    }],
                ),
                EntryPayload::Custom {
                    kind: "guard".into(),
                    data: json!({
                        "actor": "agent",
                        "action": "fs_read /home/vezir/.bashrc",
                        "decision": "deny",
                        "detail": "liegt außerhalb der Policy für agent"
                    }),
                },
                EntryPayload::Custom {
                    kind: "subagent".into(),
                    data: json!({
                        "kind": "subagent",
                        "tool": "task",
                        "session": "7c1e0000-0000-0000-0000-000000000000",
                        "task": "prüfe die Tests",
                        "entries": 2
                    }),
                },
                EntryPayload::Custom {
                    kind: "aborted".into(),
                    data: json!({ "reason": "missing_api_key" }),
                },
                EntryPayload::Compaction {
                    summary: "Bisher: README gelesen".into(),
                    replaced_until: "x".into(),
                },
                EntryPayload::Custom {
                    kind: "unbekannt".into(),
                    data: json!({ "x": 1 }),
                },
            ],
        );
        root.children.insert(child.id.clone(), child);
        root
    }

    #[test]
    fn renders_every_entry_kind_in_order() {
        let text = render_audit(&full_view());
        let lines: Vec<&str> = text.lines().collect();

        assert!(lines[0].starts_with("Sitzung 3f2a0000"), "{}", lines[0]);
        assert_eq!(lines[1], "Verzeichnis /projekt");

        let body: Vec<&str> = lines.iter().skip(3).copied().collect();
        assert!(body[0].contains("Nutzer") && body[0].contains("lies README.md"));
        assert!(body[1].contains("Modell") && body[1].contains("Ich schaue nach."));
        assert!(body[2].contains("Tool →") && body[2].contains("read"));
        assert!(body[3].contains("Guard") && body[3].contains("ALLOW"));
        assert!(body[4].contains("Tool ✓") && body[4].contains("# sepp mini"));
        assert!(
            body[5].contains("DENY") && body[5].contains("außerhalb der Policy"),
            "{}",
            body[5]
        );
        assert!(body[6].contains("Sub-Agent") && body[6].contains("7c1e0000"));
        // Kind-Session eingerückt direkt danach.
        assert!(body[7].starts_with("  ") && body[7].contains("prüfe die Tests"));
        assert!(body[8].starts_with("  ") && body[8].contains("alles grün"));
        assert!(body[9].contains("Abbruch") && body[9].contains("missing_api_key"));
        assert!(body[10].contains("Kompakt"));
        assert!(body[11].contains("[unbekannt]"));

        assert!(
            text.contains("2 Prompts · 1 Tool-Aufrufe · 1 verweigert · 1 Sub-Agenten"),
            "{text}"
        );
    }

    #[test]
    fn json_output_is_one_object_per_entry() {
        let out = render_json(&full_view());
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines.len(), 9 + 2, "Wurzel- plus Kind-Einträge");
        for l in &lines {
            let v: serde_json::Value = serde_json::from_str(l).expect("gültiges JSON");
            assert!(v["session"].is_string());
            assert!(v["entry"]["payload"].is_object());
        }
        let guards = lines
            .iter()
            .filter(|l| l.contains("\"kind\":\"guard\""))
            .count();
        assert_eq!(guards, 2);
    }

    #[test]
    fn without_children_the_subagent_line_stands_alone() {
        let mut v = full_view();
        v.children.clear(); // wie --no-children
        let text = render_audit(&v);
        assert!(text.contains("Sub-Agent"));
        assert!(!text.contains("prüfe die Tests\n"), "{text}");
        assert!(!text.contains("alles grün"), "{text}");
    }

    #[test]
    fn orphan_child_is_still_shown() {
        // Kind-Session ohne Verweis-Eintrag (Sub-Agent brach ab) darf nicht verschwinden.
        let mut v = view("3f2a", None, vec![]);
        let child = view(
            "9999",
            Some("3f2a"),
            vec![msg(Role::User, vec![ContentBlock::text("halb fertig")])],
        );
        v.children.insert(child.id.clone(), child);
        let text = render_audit(&v);
        assert!(text.contains("abgebrochener Lauf"), "{text}");
        assert!(text.contains("halb fertig"), "{text}");
    }
}
