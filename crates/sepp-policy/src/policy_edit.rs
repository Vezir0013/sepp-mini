//! Schreibender Zugriff auf eine Policy-Datei — für „dauerhaft erlauben" aus dem
//! Rückfrage-Dialog ([`crate::PermissionAnswer::Always`]) und für `sepp policy allow`.
//!
//! Nutzt `toml_edit` statt `toml`: Kommentare, Reihenfolge und Formatierung der Datei bleiben
//! erhalten. Die Datei ist Nutzereigentum — es wird ausschließlich **ergänzt**, nie entfernt oder
//! umgeschrieben. Ein bereits vorhandener Wert ist ein No-op (`Ok(false)`).

use std::path::Path;

use toml_edit::{value, Array, DocumentMut, Item, Table, Value};

use sepp_core::{Result, SeppError};

use crate::guard::Actor;

/// Kopf einer frisch angelegten Policy-Datei (wenn `allow` in eine nicht existierende Datei
/// schreibt — der ausführliche Kommentarkopf kommt aus `sepp init`).
const FRESH_HEADER: &str = "# sepp mini — Sepp Guard: Rechte des Agenten und der Erweiterungen.\n\
     # Angelegt beim ersten `allow`; Einträge erweitern die eingebauten Defaults.\n\
     # Vollständige Vorlage mit allen Abschnitten: `sepp init`. Kontrolle: `sepp policy`.\n\n";

/// Rechte, die eine Policy-Datei kennt.
pub const RIGHTS: &[&str] = &["fs_read", "fs_write", "net", "env", "exec"];

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
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent)
                    .map_err(|e| SeppError::Config(format!("policy {}: {e}", parent.display())))?;
            }
        }
        let out = if fresh {
            format!("{FRESH_HEADER}{doc}")
        } else {
            doc.to_string()
        };
        std::fs::write(path, out)
            .map_err(|e| SeppError::Config(format!("policy {}: {e}", path.display())))?;
    }
    Ok(changed)
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
