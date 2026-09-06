//! Schreibender Zugriff auf eine Policy-Datei — für „dauerhaft erlauben" aus dem
//! Rückfrage-Dialog ([`crate::PermissionAnswer::Always`]) und für `sepp policy allow`.
//!
//! Nutzt `toml_edit` statt `toml`: Kommentare, Reihenfolge und Formatierung der Datei bleiben
//! erhalten. Die Datei ist Nutzereigentum — es wird ausschließlich **ergänzt**, nie entfernt oder
//! umgeschrieben. Ein bereits vorhandener Wert ist ein No-op (`Ok(false)`).
//!
//! **Die eine Ausnahme: Paketblöcke.** `sepp pkg install` schreibt die Rechte eines Pakets als
//! Block zwischen zwei Kommentarzeilen ([`PKG_BEGIN`], [`PKG_END`]); `sepp pkg remove` nimmt
//! genau diesen Block wieder heraus, ein Upgrade ersetzt ihn. Was außerhalb der Marker steht,
//! wird auch dabei nie angefasst. Die Marker sind Kommentare, keine Schlüssel: Jeder
//! Metadaten-Schlüssel würde vom Loader als „unbekannt, ohne Wirkung" gemeldet — und ein Block
//! ist im Dokumentmodell von `toml_edit` nicht darstellbar (ein Kommentar *nach* dem letzten Wert
//! hängt am nächsten Tabellenkopf und wanderte beim nächsten `allow` mit). Deshalb wird ein Block
//! als Zeilenbereich behandelt und das Ergebnis vor dem Schreiben noch einmal geparst.
//!
//! Geschrieben wird immer atomar ([`crate::fsutil::write_atomic`]); der Dateimodus bleibt.

use std::path::Path;

use toml_edit::{value, Array, DocumentMut, Item, Table, Value};

use sepp_core::{Result, SeppError};

use crate::fsutil::write_atomic;
use crate::guard::{Actor, ExecGrant, Grants, NetGrant, PolicyFile};

/// Kopf einer frisch angelegten Policy-Datei (wenn `allow` in eine nicht existierende Datei
/// schreibt — der ausführliche Kommentarkopf kommt aus `sepp init`).
const FRESH_HEADER: &str = "# sepp mini — Sepp Guard: Rechte des Agenten und der Erweiterungen.\n\
     # Angelegt beim ersten `allow`; Einträge erweitern die eingebauten Defaults.\n\
     # Vollständige Vorlage mit allen Abschnitten: `sepp init`. Kontrolle: `sepp policy`.\n\n";

/// Rechte, die eine Policy-Datei kennt.
pub const RIGHTS: &[&str] = &["fs_read", "fs_write", "net", "env", "exec"];

/// Beginn eines Paketblocks: `# von sepp pkg: <name> <version> — nicht von Hand ändern`.
pub const PKG_BEGIN: &str = "# von sepp pkg: ";
/// Ende eines Paketblocks: `# Ende sepp pkg: <name>`.
pub const PKG_END: &str = "# Ende sepp pkg: ";

/// TOML-Abschnittspfad eines Akteurs: `agent`, `mcp.<name>`, `plugin.<name>`.
fn section_path(actor: &Actor) -> Vec<String> {
    match actor {
        Actor::Agent => vec!["agent".into()],
        Actor::Mcp(n) => vec!["mcp".into(), n.clone()],
        Actor::Plugin(n) => vec!["plugin".into(), n.clone()],
    }
}

/// Parst `<akteur>` aus der Kommandozeile (`agent`, `mcp.git`, `plugin.string-tools`).
pub fn parse_actor(s: &str) -> Option<Actor> {
    match s {
        "agent" => Some(Actor::Agent),
        _ => {
            let (kind, name) = s.split_once('.')?;
            if name.is_empty() {
                return None;
            }
            match kind {
                "mcp" => Some(Actor::Mcp(name.to_string())),
                "plugin" => Some(Actor::Plugin(name.to_string())),
                _ => None,
            }
        }
    }
}

/// Holt (oder erzeugt) die Tabelle unter `path`. Zwischentabellen werden implizit angelegt,
/// damit `[mcp.git]` als eine Zeile erscheint und nicht als `[mcp]` + `[mcp.git]`.
fn table_at<'a>(doc: &'a mut DocumentMut, path: &[String]) -> Result<&'a mut Table> {
    let mut current = doc.as_table_mut();
    for (i, key) in path.iter().enumerate() {
        let last = i + 1 == path.len();
        let entry = current.entry(key).or_insert_with(|| {
            let mut t = Table::new();
            // Zwischenebenen (`mcp` in `[mcp.git]`) bleiben implizit.
            t.set_implicit(!last);
            Item::Table(t)
        });
        current = entry.as_table_mut().ok_or_else(|| {
            SeppError::Config(format!(
                "policy: '{}' ist keine Tabelle — Datei von Hand prüfen",
                path[..=i].join(".")
            ))
        })?;
    }
    Ok(current)
}

/// Ergänzt `<recht> = [… , "<wert>"]` (bzw. `net = true`) im Abschnitt des Akteurs.
/// `Ok(true)` = ergänzt, `Ok(false)` = stand schon so drin.
pub fn allow(path: &Path, actor: &Actor, right: &str, val: &str) -> Result<bool> {
    if !RIGHTS.contains(&right) {
        return Err(SeppError::Config(format!(
            "policy: unbekanntes Recht '{right}' (erlaubt: {})",
            RIGHTS.join(", ")
        )));
    }
    // Bei einer neuen Datei bleibt der Kommentarkopf AUSSERHALB des Dokuments und wird beim
    // Schreiben vorangestellt: `toml_edit` hängt Kommentare ohne folgende Zeile sonst als
    // nachlaufende Dekoration ans Ende.
    let mut fresh = false;
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            fresh = true;
            String::new()
        }
        Err(e) => return Err(SeppError::Config(format!("policy {}: {e}", path.display()))),
    };
    let mut doc: DocumentMut = text
        .parse()
        .map_err(|e| SeppError::Config(format!("policy {}: {e}", path.display())))?;

    let section = section_path(actor);
    let changed = {
        let table = table_at(&mut doc, &section)?;
        // `net = true|false` ist ein Bool, alles andere eine Liste.
        if right == "net" && (val == "true" || val == "false") {
            let want = val == "true";
            if table.get("net").and_then(|i| i.as_bool()) == Some(want) {
                false
            } else {
                table["net"] = value(want);
                true
            }
        } else {
            push_unique(table, right, val)?
        }
    };

    if changed {
        let out = if fresh {
            format!("{FRESH_HEADER}{doc}")
        } else {
            doc.to_string()
        };
        write_atomic(path, out.as_bytes(), None)?;
    }
    Ok(changed)
}

/// Liest die Datei; `NotFound` ergibt `(String::new(), true)`.
fn read_or_fresh(path: &Path) -> Result<(String, bool)> {
    match std::fs::read_to_string(path) {
        Ok(t) => Ok((t, false)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok((String::new(), true)),
        Err(e) => Err(SeppError::Config(format!("policy {}: {e}", path.display()))),
    }
}

/// Schreibt die Rechte eines Pakets als markierten Block in die Policy-Datei — je Plugin ein
/// `[plugin.<name>]`. Ein vorhandener Block desselben Pakets wird ersetzt (Upgrade); ein
/// Zielabschnitt, der **außerhalb** eines Paketblocks schon existiert, ist ein Fehler, weil zwei
/// Tabellen gleichen Namens die Datei unlesbar machten und ein stilles Zusammenführen die
/// Gewährung des Nutzers veränderte.
///
/// Erlaubt sind nur `Actor::Plugin` und Rechte ohne `exec` (Plugins haben keinen Exec-Import).
/// Leere Gewährungen werden übersprungen; ein Paket ganz ohne Rechte schreibt keinen Block.
pub fn write_package_section(
    path: &Path,
    pkg: &str,
    version: &str,
    grants: &[(Actor, Grants)],
) -> Result<()> {
    let block = render_block(pkg, version, grants)?;
    let (text, fresh) = read_or_fresh(path)?;
    let lines: Vec<&str> = text.lines().collect();
    let existing = find_block(&lines, pkg)?;

    // Resttext ohne den eigenen alten Block — gegen den wird auf Kollisionen geprüft.
    let (before, after): (Vec<&str>, Vec<&str>) = match existing {
        Some((start, end)) => {
            let head_end = strip_blank_before(&lines, start);
            (lines[..head_end].to_vec(), lines[end + 1..].to_vec())
        }
        None => (lines.clone(), Vec::new()),
    };
    let rest = join_lines(&before, &after);
    let doc: DocumentMut = rest
        .parse()
        .map_err(|e| SeppError::Config(format!("policy {}: {e}", path.display())))?;
    for (actor, g) in grants {
        let Actor::Plugin(name) = actor else {
            return Err(SeppError::Config(format!(
                "policy: Paketrechte gelten nur für Plugins, nicht für {actor}"
            )));
        };
        if g.is_empty() {
            continue;
        }
        let taken = doc
            .get("plugin")
            .and_then(|p| p.as_table_like())
            .is_some_and(|t| t.contains_key(name));
        if taken {
            return Err(SeppError::Config(format!(
                "policy {}: [plugin.{name}] ist außerhalb des Paketblocks vorhanden (von Hand \
                 oder aus einem anderen Paket) — bitte prüfen, bevor `{pkg}` installiert wird",
                path.display()
            )));
        }
    }
    if block.is_empty() {
        // Nichts zu schreiben — aber ein alter Block desselben Pakets wäre jetzt veraltet.
        if existing.is_some() {
            finish(path, &rest, fresh)?;
        }
        return Ok(());
    }

    // Block an der alten Stelle bzw. am Ende einfügen, mit genau einer Leerzeile davor.
    let mut out = String::new();
    let push_block = |out: &mut String, prefix_lines: &[&str]| {
        let head = join_lines(prefix_lines, &[]);
        out.push_str(&head);
        if !head.is_empty() {
            if !head.ends_with('\n') {
                out.push('\n');
            }
            out.push('\n');
        }
        out.push_str(&block);
    };
    match existing {
        Some(_) => {
            push_block(&mut out, &before);
            let tail = join_lines(&after, &[]);
            if !tail.trim().is_empty() {
                out.push('\n');
                out.push_str(tail.trim_start_matches('\n'));
            }
        }
        None => push_block(&mut out, &lines),
    }
    finish(path, &out, fresh)
}

/// Entfernt den markierten Block des Pakets. `Ok(false)`, wenn keiner da ist. Unvollständige
/// Marker (Beginn ohne Ende, Ende ohne Beginn, doppelter Beginn) sind ein Fehler — dann hat
/// jemand von Hand geändert, und Raten wäre falsch.
pub fn remove_package_section(path: &Path, pkg: &str) -> Result<bool> {
    let (text, fresh) = read_or_fresh(path)?;
    if fresh {
        return Ok(false);
    }
    let lines: Vec<&str> = text.lines().collect();
    let Some((start, end)) = find_block(&lines, pkg)? else {
        return Ok(false);
    };
    let head_end = strip_blank_before(&lines, start);
    let out = join_lines(&lines[..head_end], &lines[end + 1..]);
    finish(path, &out, false)?;
    Ok(true)
}

/// Namen und Versionen aller Paketblöcke in der Datei (für `sepp pkg list` und Diagnosen).
pub fn package_sections(path: &Path) -> Result<Vec<(String, String)>> {
    let (text, _) = read_or_fresh(path)?;
    let mut out = Vec::new();
    for line in text.lines() {
        if let Some(rest) = line.trim_end().strip_prefix(PKG_BEGIN) {
            let mut it = rest.split_whitespace();
            if let (Some(name), Some(version)) = (it.next(), it.next()) {
                out.push((name.to_string(), version.to_string()));
            }
        }
    }
    Ok(out)
}

/// Sucht den Block `pkg` als Zeilenbereich (inklusive beider Markerzeilen).
fn find_block(lines: &[&str], pkg: &str) -> Result<Option<(usize, usize)>> {
    let begin_prefix = format!("{PKG_BEGIN}{pkg} ");
    let end_line = format!("{PKG_END}{pkg}");
    let mut start = None;
    let mut end = None;
    for (i, line) in lines.iter().enumerate() {
        let t = line.trim_end();
        if t.starts_with(&begin_prefix) {
            if start.is_some() {
                return Err(marker_error(pkg, "der Beginn steht zweimal"));
            }
            start = Some(i);
        } else if t == end_line {
            if end.is_some() {
                return Err(marker_error(pkg, "das Ende steht zweimal"));
            }
            end = Some(i);
        }
    }
    match (start, end) {
        (None, None) => Ok(None),
        (Some(s), Some(e)) if s < e => Ok(Some((s, e))),
        (Some(_), Some(_)) => Err(marker_error(pkg, "das Ende steht vor dem Beginn")),
        (Some(_), None) => Err(marker_error(pkg, "der Beginn hat kein Ende")),
        (None, Some(_)) => Err(marker_error(pkg, "das Ende hat keinen Beginn")),
    }
}

fn marker_error(pkg: &str, what: &str) -> SeppError {
    SeppError::Config(format!(
        "policy: die Marker des Pakets `{pkg}` sind unvollständig ({what}) — Datei von Hand \
         prüfen; erwartet werden `{PKG_BEGIN}{pkg} <version> …` und `{PKG_END}{pkg}`"
    ))
}

/// Der Index, bis zu dem der Text vor `start` reicht, ohne genau die eine Leerzeile, die der
/// Block sich selbst vorangestellt hat.
fn strip_blank_before(lines: &[&str], start: usize) -> usize {
    if start > 0 && lines[start - 1].trim().is_empty() {
        start - 1
    } else {
        start
    }
}

fn join_lines(a: &[&str], b: &[&str]) -> String {
    let mut s = String::new();
    for l in a.iter().chain(b.iter()) {
        s.push_str(l);
        s.push('\n');
    }
    s
}

/// Der Block als Text: Beginnzeile, die Tabellen, Endzeile. Leer, wenn keine Gewährung bleibt.
fn render_block(pkg: &str, version: &str, grants: &[(Actor, Grants)]) -> Result<String> {
    if pkg.contains(char::is_whitespace) || version.contains(char::is_whitespace) {
        return Err(SeppError::Config(
            "policy: Paketname und Version dürfen keine Leerzeichen enthalten".into(),
        ));
    }
    let mut doc = DocumentMut::new();
    let mut any = false;
    for (actor, g) in grants {
        let Actor::Plugin(_) = actor else {
            return Err(SeppError::Config(format!(
                "policy: Paketrechte gelten nur für Plugins, nicht für {actor}"
            )));
        };
        if g.exec != ExecGrant::Unset {
            return Err(SeppError::Config(format!(
                "policy: `exec` ist für Plugins unzulässig ({actor})"
            )));
        }
        if g.is_empty() {
            continue;
        }
        any = true;
        let table = table_at(&mut doc, &section_path(actor))?;
        for p in &g.fs_read {
            push_unique(table, "fs_read", p)?;
        }
        for p in &g.fs_write {
            push_unique(table, "fs_write", p)?;
        }
        match &g.net {
            NetGrant::Off => {}
            NetGrant::All => table["net"] = value(true),
            NetGrant::Hosts(hosts) => {
                for h in hosts {
                    push_unique(table, "net", h)?;
                }
            }
        }
        for e in &g.env {
            push_unique(table, "env", e)?;
        }
    }
    if !any {
        return Ok(String::new());
    }
    let body = doc.to_string();
    let body = body.trim_matches('\n');
    Ok(format!(
        "{PKG_BEGIN}{pkg} {version} — nicht von Hand ändern\n{body}\n{PKG_END}{pkg}\n"
    ))
}

/// Prüft das Ergebnis (parst es wie der Loader) und schreibt atomar.
fn finish(path: &Path, text: &str, fresh: bool) -> Result<()> {
    let out = if fresh {
        format!("{FRESH_HEADER}{text}")
    } else {
        text.to_string()
    };
    PolicyFile::parse(&out).map_err(|e| {
        SeppError::Config(format!(
            "policy {}: das Ergebnis wäre nicht lesbar ({e}) — nichts geschrieben",
            path.display()
        ))
    })?;
    write_atomic(path, out.as_bytes(), None)
}

/// Hängt `val` an das Array `key` an (legt es an, wenn nötig); Duplikate werden übersprungen.
fn push_unique(table: &mut Table, key: &str, val: &str) -> Result<bool> {
    match table.get_mut(key) {
        None => {
            let mut arr = Array::new();
            arr.push(val);
            arr.fmt();
            table[key] = Item::Value(Value::Array(arr));
            Ok(true)
        }
        Some(item) => {
            let arr = item.as_array_mut().ok_or_else(|| {
                SeppError::Config(format!(
                    "policy: '{key}' ist keine Liste (steht dort z. B. \"system\" oder true?) — \
                     Datei von Hand anpassen"
                ))
            })?;
            if arr.iter().any(|v| v.as_str() == Some(val)) {
                return Ok(false);
            }
            arr.push(val);
            arr.fmt();
            Ok(true)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_file(name: &str, content: &str) -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join(name);
        if !content.is_empty() {
            std::fs::write(&p, content).unwrap();
        }
        (dir, p)
    }

    #[test]
    fn allow_appends_and_keeps_comments() {
        let original = "# Kopfkommentar bleibt\n\
                        mode = \"ask\"\n\n\
                        [agent]\n\
                        # Kommentar im Abschnitt\n\
                        fs_read = [\"~/.cargo\"]\n";
        let (_d, p) = tmp_file("policy.toml", original);
        assert!(allow(&p, &Actor::Agent, "fs_read", "/srv/data").unwrap());
        let after = std::fs::read_to_string(&p).unwrap();
        assert!(after.contains("# Kopfkommentar bleibt"), "{after}");
        assert!(after.contains("# Kommentar im Abschnitt"), "{after}");
        assert!(after.contains("mode = \"ask\""), "{after}");
        assert!(
            after.contains("fs_read = [\"~/.cargo\", \"/srv/data\"]"),
            "{after}"
        );
        // Zweiter Aufruf mit demselben Wert ist ein No-op.
        assert!(!allow(&p, &Actor::Agent, "fs_read", "/srv/data").unwrap());
        assert_eq!(after, std::fs::read_to_string(&p).unwrap());
        // Ergebnis bleibt parsebar und wirksam.
        let f = crate::PolicyFile::parse(&std::fs::read_to_string(&p).unwrap()).unwrap();
        let g = f.agent.unwrap().grants;
        assert_eq!(
            g.fs_read,
            vec!["~/.cargo".to_string(), "/srv/data".to_string()]
        );
    }

    #[test]
    fn allow_creates_file_section_and_field() {
        let (_d, p) = tmp_file("policy.toml", "");
        assert!(allow(&p, &Actor::Agent, "fs_write", "/out").unwrap());
        assert!(allow(&p, &Actor::Mcp("git".into()), "exec", "git").unwrap());
        assert!(allow(&p, &Actor::Plugin("st".into()), "net", "api.example.com").unwrap());
        let after = std::fs::read_to_string(&p).unwrap();
        assert!(after.starts_with("# sepp mini"), "{after}");
        assert!(after.contains("[agent]"), "{after}");
        assert!(after.contains("[mcp.git]"), "{after}");
        assert!(
            !after.contains("[mcp]\n"),
            "Zwischenebene implizit: {after}"
        );
        assert!(after.contains("[plugin.st]"), "{after}");
        let f = crate::PolicyFile::parse(&after).unwrap();
        assert_eq!(f.agent.unwrap().grants.fs_write, vec!["/out".to_string()]);
        assert_eq!(
            f.mcp["git"].exec,
            crate::ExecGrant::Programs(vec!["git".into()])
        );
        assert_eq!(
            f.plugin["st"].net,
            crate::NetGrant::Hosts(vec!["api.example.com".into()])
        );
    }

    #[test]
    fn allow_sets_net_bool() {
        let (_d, p) = tmp_file("policy.toml", "[agent]\nnet = false\n");
        assert!(allow(&p, &Actor::Agent, "net", "true").unwrap());
        assert!(std::fs::read_to_string(&p).unwrap().contains("net = true"));
        assert!(!allow(&p, &Actor::Agent, "net", "true").unwrap());
        // Host-Liste in eine bestehende Bool-Zeile → klarer Fehler statt stiller Umbau.
        let err = allow(&p, &Actor::Agent, "net", "example.com").unwrap_err();
        assert!(err.to_string().contains("keine Liste"), "{err}");
    }

    #[test]
    fn allow_rejects_unknown_right_and_non_list_field() {
        let (_d, p) = tmp_file("policy.toml", "[agent]\nexec = \"system\"\n");
        assert!(allow(&p, &Actor::Agent, "quatsch", "x").is_err());
        let err = allow(&p, &Actor::Agent, "exec", "git").unwrap_err();
        assert!(err.to_string().contains("keine Liste"), "{err}");
    }

    fn pkg_grants(net_all: bool) -> Vec<(Actor, Grants)> {
        let mut g = Grants {
            fs_read: vec!["/home/anna/belege".into()],
            env: vec!["ACME_TOKEN".into()],
            ..Grants::default()
        };
        g.net = if net_all {
            NetGrant::All
        } else {
            NetGrant::Hosts(vec!["api.example.com".into()])
        };
        vec![(Actor::Plugin("pdf_extract".into()), g)]
    }

    #[test]
    fn package_block_writes_parses_and_removes_byte_identically() {
        let original = "# Kopf\nmode = \"ask\"\n\n[agent]\nfs_read = [\"~/.cargo\"]\n\n[plugin.eigenes]\nnet = true\n";
        let (_d, p) = tmp_file("policy.toml", original);
        write_package_section(&p, "rechnung", "1.0.0", &pkg_grants(false)).unwrap();
        let after = std::fs::read_to_string(&p).unwrap();
        assert!(
            after.contains(
                "\n# von sepp pkg: rechnung 1.0.0 — nicht von Hand ändern\n[plugin.pdf_extract]\n"
            ),
            "{after}"
        );
        assert!(after.ends_with("\n# Ende sepp pkg: rechnung\n"), "{after}");
        assert!(
            after.starts_with(original),
            "Nutzerteil unverändert: {after}"
        );
        let f = PolicyFile::parse(&after).unwrap();
        let g = &f.plugin["pdf_extract"];
        assert_eq!(g.fs_read, vec!["/home/anna/belege".to_string()]);
        assert_eq!(g.net, NetGrant::Hosts(vec!["api.example.com".into()]));
        assert_eq!(g.env, vec!["ACME_TOKEN".to_string()]);
        assert_eq!(
            f.plugin["eigenes"].net,
            NetGrant::All,
            "Nutzerabschnitt bleibt"
        );
        assert_eq!(
            package_sections(&p).unwrap(),
            vec![("rechnung".to_string(), "1.0.0".to_string())]
        );

        assert!(remove_package_section(&p, "rechnung").unwrap());
        assert_eq!(
            std::fs::read_to_string(&p).unwrap(),
            original,
            "byte-identisch"
        );
        assert!(!remove_package_section(&p, "rechnung").unwrap());
    }

    #[test]
    fn package_block_upgrade_replaces_in_place_and_keeps_following_text() {
        let (_d, p) = tmp_file("policy.toml", "[agent]\nnet = true\n");
        write_package_section(&p, "rechnung", "1.0.0", &pkg_grants(false)).unwrap();
        // Der Nutzer schreibt danach noch etwas hinter den Block.
        let mut text = std::fs::read_to_string(&p).unwrap();
        text.push_str("\n[mcp.git]\nexec = [\"git\"]\n");
        std::fs::write(&p, &text).unwrap();

        write_package_section(&p, "rechnung", "1.1.0", &pkg_grants(true)).unwrap();
        let after = std::fs::read_to_string(&p).unwrap();
        assert_eq!(after.matches(PKG_BEGIN).count(), 1, "{after}");
        assert!(after.contains("rechnung 1.1.0"), "{after}");
        assert!(!after.contains("1.0.0"), "{after}");
        let f = PolicyFile::parse(&after).unwrap();
        assert_eq!(f.plugin["pdf_extract"].net, NetGrant::All);
        assert_eq!(f.mcp["git"].exec, ExecGrant::Programs(vec!["git".into()]));
        // Reihenfolge: Block an der alten Stelle, [mcp.git] danach.
        assert!(
            after.find(PKG_END).unwrap() < after.find("[mcp.git]").unwrap(),
            "{after}"
        );
    }

    #[test]
    fn package_block_refuses_handwritten_actor_and_exec() {
        let (_d, p) = tmp_file("policy.toml", "[plugin.pdf_extract]\nnet = true\n");
        let err = write_package_section(&p, "rechnung", "1.0.0", &pkg_grants(false)).unwrap_err();
        assert!(
            err.to_string().contains("außerhalb des Paketblocks"),
            "{err}"
        );
        assert_eq!(
            std::fs::read_to_string(&p).unwrap(),
            "[plugin.pdf_extract]\nnet = true\n",
            "nichts geschrieben"
        );

        let mut g = pkg_grants(false);
        g[0].1.exec = ExecGrant::Programs(vec!["sh".into()]);
        let (_d2, q) = tmp_file("policy.toml", "");
        let err = write_package_section(&q, "rechnung", "1.0.0", &g).unwrap_err();
        assert!(err.to_string().contains("exec"), "{err}");
        let err =
            write_package_section(&q, "x", "1", &[(Actor::Agent, Grants::default())]).unwrap_err();
        assert!(err.to_string().contains("nur für Plugins"), "{err}");
    }

    #[test]
    fn package_block_refuses_broken_markers() {
        let broken = "[agent]\nnet = true\n\n# von sepp pkg: rechnung 1.0.0 — nicht von Hand ändern\n[plugin.pdf_extract]\nnet = true\n";
        let (_d, p) = tmp_file("policy.toml", broken);
        let err = remove_package_section(&p, "rechnung").unwrap_err();
        assert!(err.to_string().contains("kein Ende"), "{err}");
        let err = write_package_section(&p, "rechnung", "1.0.1", &pkg_grants(false)).unwrap_err();
        assert!(err.to_string().contains("unvollständig"), "{err}");
        assert_eq!(std::fs::read_to_string(&p).unwrap(), broken);
    }

    #[test]
    fn package_block_on_fresh_file_gets_the_header_and_empty_grants_write_nothing() {
        let (_d, p) = tmp_file("policy.toml", "");
        write_package_section(
            &p,
            "leer",
            "1.0.0",
            &[(Actor::Plugin("x".into()), Grants::default())],
        )
        .unwrap();
        assert!(!p.exists(), "ohne Rechte kein Block, keine Datei");

        write_package_section(&p, "rechnung", "1.0.0", &pkg_grants(true)).unwrap();
        let after = std::fs::read_to_string(&p).unwrap();
        assert!(after.starts_with("# sepp mini"), "{after}");
        assert!(
            after.contains("[plugin.pdf_extract]\nfs_read = [\"/home/anna/belege\"]\nnet = true\n"),
            "{after}"
        );
        PolicyFile::parse(&after).unwrap();
    }

    #[test]
    fn allow_writes_atomically_without_leftovers() {
        let (_d, p) = tmp_file("policy.toml", "[agent]\n");
        assert!(allow(&p, &Actor::Agent, "env", "FOO").unwrap());
        let leftovers: Vec<_> = std::fs::read_dir(p.parent().unwrap())
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.contains(".tmp-"))
            .collect();
        assert!(leftovers.is_empty(), "{leftovers:?}");
    }

    #[test]
    fn parse_actor_forms() {
        assert_eq!(parse_actor("agent"), Some(Actor::Agent));
        assert_eq!(parse_actor("mcp.git"), Some(Actor::Mcp("git".into())));
        assert_eq!(
            parse_actor("plugin.string-tools"),
            Some(Actor::Plugin("string-tools".into()))
        );
        assert_eq!(parse_actor("mcp."), None);
        assert_eq!(parse_actor("was.git"), None);
        assert_eq!(parse_actor("mcp"), None);
    }
}
