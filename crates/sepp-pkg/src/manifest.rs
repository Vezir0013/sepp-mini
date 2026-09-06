//! Das Paket-Manifest (`manifest.toml` im Archiv) — Selbstbeschreibung, Dateiliste mit Hashes,
//! Variablen und die Rechte, um die das Paket bittet.
//!
//! Anders als das Plugin-Manifest (`sepp_policy::Manifest`, die Selbstauskunft *eines* Moduls)
//! beschreibt es das ganze Bündel. `[rights.<plugin>]` ist die Bitte an den Nutzer und darf nicht
//! mehr fordern, als das Plugin-Manifest deklariert — geprüft in `install`.

use std::collections::{BTreeMap, BTreeSet};

use serde::Deserialize;

use sepp_core::{Result, SeppError};
use sepp_policy::{ExecGrant, Grants};

use crate::{validate_name, validate_rel_path, FORMAT};

/// Wer das Paket herausgibt und mit welchem Schlüssel er signiert.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Publisher {
    /// Namensregel wie beim Paket; Dateiname unter `trusted-keys/`.
    pub name: String,
    /// Ed25519-Public-Key, base64 (32 Byte). Trägt `pack` ein.
    pub key: String,
}

/// Art eines Platzhalters: ein Pfad wird bei der Installation aufgelöst und absolut geschrieben.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum VarKind {
    #[default]
    String,
    Path,
}

/// Ein Platzhalter `${NAME}`, den das Paket in seinen Rechten verwendet.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct VarSpec {
    pub description: String,
    #[serde(default)]
    pub default: Option<String>,
    #[serde(default)]
    pub kind: VarKind,
}

/// Das Paket-Manifest.
#[derive(Debug, Clone, Deserialize)]
pub struct PkgManifest {
    #[serde(default = "format_v1")]
    pub format: u32,
    pub name: String,
    pub version: String,
    #[serde(default)]
    pub description: Option<String>,
    pub publisher: Publisher,
    /// `"<rel/pfad>" = "<sha256 hex>"` — schreibt `pack`, im Quellverzeichnis verboten.
    #[serde(default)]
    pub files: BTreeMap<String, String>,
    #[serde(default)]
    pub vars: BTreeMap<String, VarSpec>,
    /// Schlüssel = Plugin-Stamm (`plugins/<stamm>.wasm`).
    #[serde(default)]
    pub rights: BTreeMap<String, Grants>,
    /// Unbekannte Felder: gelesen, gemeldet, ignoriert — ein neueres Paket scheitert nicht auf
    /// einem älteren `sepp`, aber der Nutzer erfährt, dass etwas ohne Wirkung ist.
    #[serde(flatten)]
    pub unknown: BTreeMap<String, toml::Value>,
}

fn format_v1() -> u32 {
    1
}

/// Was im Paket steckt, abgeleitet aus `[files]` — für Zustimmungsdialog und Nachweis.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Inventory {
    pub skills: Vec<String>,
    pub prompts: Vec<String>,
    /// Dateinamen der Hooks (`hooks/<name>.rhai`).
    pub hooks: Vec<String>,
    /// Plugin-Stämme (`plugins/<stamm>.wasm`).
    pub plugins: Vec<String>,
}

impl PkgManifest {
    /// Parst das Manifest, ohne es zu validieren — `validate` ist der zweite Schritt, damit ein
    /// Aufrufer beide Fehlerarten unterscheiden kann.
    pub fn parse(text: &str) -> Result<Self> {
        toml::from_str(text).map_err(|e| SeppError::Config(format!("pkg manifest: {e}")))
    }

    /// Namen der Felder, die dieser Host nicht kennt (sortiert).
    pub fn unknown_keys(&self) -> Vec<&str> {
        self.unknown.keys().map(String::as_str).collect()
    }

    /// Prüft alles, was sich ohne Archiv prüfen lässt. Fail-closed: lieber ein klarer Fehler als
    /// ein Paket, das halb passt.
    pub fn validate(&self) -> Result<()> {
        if self.format > FORMAT {
            return Err(SeppError::Config(format!(
                "pkg: Paket hat Format {}, dieser sepp liest {FORMAT} — neueres sepp nötig",
                self.format
            )));
        }
        if self.format == 0 {
            return Err(SeppError::Config("pkg: `format` muss ≥ 1 sein".into()));
        }
        validate_name(&self.name)?;
        validate_name(&self.publisher.name)
            .map_err(|e| SeppError::Config(format!("{e} (publisher.name)")))?;
        semver::Version::parse(&self.version).map_err(|e| {
            SeppError::Config(format!(
                "pkg: `version` {:?} ist keine semver-Version ({e})",
                self.version
            ))
        })?;
        crate::crypto::decode_pubkey(&self.publisher.key)?;

        for (path, hash) in &self.files {
            validate_rel_path(path)?;
            if hash.len() != 64 || crate::unhex(hash).is_none() {
                return Err(SeppError::Config(format!(
                    "pkg: [files] {path:?}: kein SHA-256 (64 Hex-Zeichen erwartet)"
                )));
            }
        }
        for name in self.vars.keys() {
            let ok = name.chars().next().is_some_and(|c| c.is_ascii_uppercase())
                && name
                    .chars()
                    .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
                && name.len() <= 32;
            if !ok {
                return Err(SeppError::Config(format!(
                    "pkg: [vars] {name:?}: erlaubt sind 1 bis 32 Zeichen aus A-Z, 0-9 und _, \
                     beginnend mit einem Buchstaben"
                )));
            }
        }
        let inv = self.inventory();
        for (plugin, grants) in &self.rights {
            if !inv.plugins.contains(plugin) {
                return Err(SeppError::Config(format!(
                    "pkg: [rights.{plugin}] nennt ein Plugin, das nicht unter plugins/ liegt \
                     (erwartet plugins/{plugin}.wasm und plugins/{plugin}.toml)"
                )));
            }
            if grants.exec != ExecGrant::Unset {
                return Err(SeppError::Config(format!(
                    "pkg: [rights.{plugin}]: `exec` ist für Plugins unzulässig"
                )));
            }
            let unknown = grants.unknown_keys();
            if !unknown.is_empty() {
                return Err(SeppError::Config(format!(
                    "pkg: [rights.{plugin}]: unbekannte Rechte {}",
                    unknown.join(", ")
                )));
            }
        }
        for var in self.referenced_vars() {
            if !self.vars.contains_key(&var) {
                return Err(SeppError::Config(format!(
                    "pkg: ${{{var}}} wird in [rights] benutzt, aber nicht unter [vars] erklärt"
                )));
            }
        }
        // Jedes Plugin braucht beide Dateien; ein Manifest neben dem Modul ist Pflicht, weil
        // sonst `abi` stillschweigend als 1 gälte.
        for stem in &inv.plugins {
            if !self.files.contains_key(&format!("plugins/{stem}.toml")) {
                return Err(SeppError::Config(format!(
                    "pkg: plugins/{stem}.wasm hat kein Manifest plugins/{stem}.toml"
                )));
            }
        }
        for path in self.files.keys() {
            if let Some(rest) = path.strip_prefix("plugins/") {
                if rest.contains('/') {
                    return Err(SeppError::Config(format!(
                        "pkg: {path:?}: unter plugins/ sind nur <name>.wasm und <name>.toml \
                         erlaubt, keine Unterverzeichnisse"
                    )));
                }
                if !(rest.ends_with(".wasm") || rest.ends_with(".toml")) {
                    return Err(SeppError::Config(format!(
                        "pkg: {path:?}: unter plugins/ sind nur .wasm und .toml erlaubt"
                    )));
                }
                if rest == "manifest.toml" {
                    return Err(SeppError::Config(
                        "pkg: plugins/manifest.toml ist nicht erlaubt — je Plugin ein \
                         <name>.toml"
                            .into(),
                    ));
                }
            }
        }
        Ok(())
    }

    /// Leitet aus `[files]` ab, was das Paket enthält.
    pub fn inventory(&self) -> Inventory {
        let mut inv = Inventory::default();
        for path in self.files.keys() {
            let mut parts = path.splitn(3, '/');
            let (Some(top), Some(second)) = (parts.next(), parts.next()) else {
                continue;
            };
            match top {
                "skills" => {
                    // `skills/<name>/SKILL.md` oder `skills/<name>.md`
                    let name = match parts.next() {
                        Some("SKILL.md") => second.to_string(),
                        Some(_) => continue,
                        None => match second.strip_suffix(".md") {
                            Some(n) => n.to_string(),
                            None => continue,
                        },
                    };
                    if !inv.skills.contains(&name) {
                        inv.skills.push(name);
                    }
                }
                "prompts" if parts.next().is_none() => {
                    if let Some(n) = second.strip_suffix(".md") {
                        inv.prompts.push(n.to_string());
                    }
                }
                "hooks" if parts.next().is_none() && second.ends_with(".rhai") => {
                    inv.hooks.push(second.to_string());
                }
                "plugins" if parts.next().is_none() => {
                    if let Some(n) = second.strip_suffix(".wasm") {
                        inv.plugins.push(n.to_string());
                    }
                }
                _ => {}
            }
        }
        inv
    }

    /// Alle `${NAME}`, die in `[rights]` vorkommen.
    pub fn referenced_vars(&self) -> BTreeSet<String> {
        let mut out = BTreeSet::new();
        for grants in self.rights.values() {
            for s in grants
                .fs_read
                .iter()
                .chain(&grants.fs_write)
                .chain(&grants.env)
            {
                collect_vars(s, &mut out);
            }
            if let sepp_policy::NetGrant::Hosts(hosts) = &grants.net {
                for h in hosts {
                    collect_vars(h, &mut out);
                }
            }
        }
        out
    }
}

/// Sammelt `${NAME}`-Vorkommen; ein `$` ohne Klammer ist kein Platzhalter.
pub(crate) fn collect_vars(s: &str, out: &mut BTreeSet<String>) {
    let mut rest = s;
    while let Some(i) = rest.find("${") {
        let after = &rest[i + 2..];
        match after.find('}') {
            Some(j) => {
                out.insert(after[..j].to_string());
                rest = &after[j + 1..];
            }
            None => break,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    pub(crate) const KEY: &str = "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA="; // 32 Nullbytes
    const H: &str = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";

    fn example() -> String {
        format!(
            r#"
format = 1
name = "rechnung"
version = "1.0.0"
description = "Rechnungen prüfen"

[publisher]
name = "acme"
key = "{KEY}"

[vars.BELEGE_DIR]
description = "Ordner mit den Belegen"
kind = "path"
default = "~/buchhaltung"

[rights.pdf_extract]
fs_read = ["${{BELEGE_DIR}}"]
net = ["api.example.com"]
env = ["ACME_TOKEN"]

[files]
"skills/rechnung/SKILL.md" = "{H}"
"skills/kurz.md" = "{H}"
"prompts/pruefen.md" = "{H}"
"hooks/log.rhai" = "{H}"
"plugins/pdf_extract.wasm" = "{H}"
"plugins/pdf_extract.toml" = "{H}"
"README.md" = "{H}"
"#
        )
    }

    #[test]
    fn example_parses_validates_and_inventories() {
        let m = PkgManifest::parse(&example()).unwrap();
        m.validate().unwrap();
        assert!(m.unknown_keys().is_empty());
        assert_eq!(
            m.inventory(),
            Inventory {
                skills: vec!["kurz".into(), "rechnung".into()],
                prompts: vec!["pruefen".into()],
                hooks: vec!["log.rhai".into()],
                plugins: vec!["pdf_extract".into()],
            }
        );
        assert_eq!(
            m.referenced_vars().into_iter().collect::<Vec<_>>(),
            vec!["BELEGE_DIR".to_string()]
        );
    }

    #[test]
    fn unknown_keys_are_reported_not_rejected() {
        let text = example().replace("format = 1", "format = 1\nzukunft = true");
        let m = PkgManifest::parse(&text).unwrap();
        assert_eq!(m.unknown_keys(), vec!["zukunft"]);
        m.validate().unwrap();
    }

    #[test]
    fn validate_rejects_each_broken_field() {
        let cases: &[(&str, &str, &str)] = &[
            ("format = 1", "format = 2", "neueres sepp"),
            ("name = \"rechnung\"", "name = \"Rechnung\"", "unzulässig"),
            ("version = \"1.0.0\"", "version = \"eins\"", "semver"),
            ("name = \"acme\"", "name = \"ACME\"", "publisher.name"),
            (KEY, "AAAA", "Schlüssel"),
            ("\"README.md\"", "\"../README.md\"", "relativ"),
            (H, "abc", "SHA-256"),
            ("[vars.BELEGE_DIR]", "[vars.belege]", "[vars]"),
            (
                "[rights.pdf_extract]",
                "[rights.anderes]",
                "nicht unter plugins/",
            ),
            (
                "env = [\"ACME_TOKEN\"]",
                "env = [\"ACME_TOKEN\"]\nexec = [\"sh\"]",
                "exec",
            ),
            (
                "env = [\"ACME_TOKEN\"]",
                "env = [\"${UNBEKANNT}\"]",
                "nicht unter [vars]",
            ),
            (
                "\"plugins/pdf_extract.toml\" = ",
                "\"plugins/pdf_extract.txt\" = ",
                "Manifest",
            ),
        ];
        for (from, to, expect) in cases {
            let text = example().replacen(from, to, 1);
            let m = PkgManifest::parse(&text).expect(from);
            let e = m.validate().expect_err(from).to_string();
            assert!(e.contains(expect), "{from} → {to}: {e}");
        }
    }

    #[test]
    fn collect_vars_finds_only_braced_placeholders() {
        let mut out = BTreeSet::new();
        collect_vars("${A}/x/${B_2}/$C/${", &mut out);
        assert_eq!(out.into_iter().collect::<Vec<_>>(), vec!["A", "B_2"]);
    }
}
