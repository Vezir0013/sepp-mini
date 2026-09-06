//! Variablen eines Pakets (`${NAME}`) und die Auflösung seiner Rechte in absolute Pfade.
//!
//! Ein Paket weiß nicht, wo beim Nutzer die Belege liegen — es fragt. Die Antwort wird bei der
//! Installation **einmal** aufgelöst und absolut in die Policy geschrieben, denn
//! `resolve_path_with` löst relative Pfade zur Laufzeit gegen das Arbeitsverzeichnis des
//! `sepp`-Prozesses auf: Ein `./belege` in der globalen `policy.toml` wäre je nach Aufrufort ein
//! anderes Recht.

use std::collections::BTreeMap;

use sepp_core::{Result, SeppError};
use sepp_policy::{canonicalize_lenient, resolve_path_with, Grants, NetGrant, ResolveCtx};

use crate::manifest::{PkgManifest, VarKind, VarSpec};

/// Aufgelöste Variablen und die, die noch fehlen.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Resolved {
    pub values: BTreeMap<String, String>,
    /// Weder angegeben, noch aus einer früheren Installation, noch mit Default.
    pub missing: Vec<(String, VarSpec)>,
}

/// Bestimmt die Werte aller deklarierten Variablen: `given` (Kommandozeile oder Dialog) schlägt
/// `previous` (frühere Installation, beim Upgrade) schlägt `default`. Unbekannte Namen in
/// `given` sind ein Fehler — ein Tippfehler soll nicht stumm ins Leere laufen.
pub fn resolve_vars(
    manifest: &PkgManifest,
    given: &BTreeMap<String, String>,
    previous: Option<&BTreeMap<String, String>>,
) -> Result<Resolved> {
    for name in given.keys() {
        if !manifest.vars.contains_key(name) {
            return Err(SeppError::Config(format!(
                "pkg: Variable {name} ist im Paket nicht deklariert (bekannt: {})",
                if manifest.vars.is_empty() {
                    "keine".to_string()
                } else {
                    manifest.vars.keys().cloned().collect::<Vec<_>>().join(", ")
                }
            )));
        }
    }
    let mut out = Resolved::default();
    for (name, spec) in &manifest.vars {
        let value = given
            .get(name)
            .cloned()
            .or_else(|| previous.and_then(|p| p.get(name).cloned()))
            .or_else(|| spec.default.clone());
        match value {
            Some(v) if !v.trim().is_empty() => {
                out.values.insert(name.clone(), v);
            }
            _ => out.missing.push((name.clone(), spec.clone())),
        }
    }
    Ok(out)
}

/// Ersetzt jedes `${NAME}` in `s`; ein unbekannter Name ist ein Fehler.
pub fn substitute(s: &str, values: &BTreeMap<String, String>) -> Result<String> {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(i) = rest.find("${") {
        out.push_str(&rest[..i]);
        let after = &rest[i + 2..];
        let Some(j) = after.find('}') else {
            return Err(SeppError::Config(format!(
                "pkg: unvollständiger Platzhalter in {s:?}"
            )));
        };
        let name = &after[..j];
        let Some(v) = values.get(name) else {
            return Err(SeppError::Config(format!(
                "pkg: Variable {name} hat keinen Wert"
            )));
        };
        out.push_str(v);
        rest = &after[j + 1..];
    }
    out.push_str(rest);
    Ok(out)
}

/// Die Rechte aus `[rights]` mit eingesetzten Variablen — Pfade werden wie bei den eingebauten
/// Tools aufgelöst (`~`, `$TMPDIR`, relativ zu `ctx.cwd`) und dann über `canonicalize_lenient`
/// absolut gemacht; das funktioniert auch für ein Verzeichnis, das der Nutzer erst noch anlegt.
/// Hosts und Variablennamen werden nur eingesetzt, nicht verändert.
pub fn resolve_rights(
    manifest: &PkgManifest,
    values: &BTreeMap<String, String>,
    ctx: &ResolveCtx,
) -> Result<Vec<(String, Grants)>> {
    let mut out = Vec::new();
    for (plugin, g) in &manifest.rights {
        let paths = |list: &[String]| -> Result<Vec<String>> {
            let mut v = Vec::new();
            for p in list {
                let s = substitute(p, values)?;
                let abs = canonicalize_lenient(&resolve_path_with(&s, ctx));
                let s = abs.to_str().map(|s| s.to_string()).ok_or_else(|| {
                    SeppError::Config(format!("pkg: Pfad {} ist kein UTF-8", abs.display()))
                })?;
                if !v.contains(&s) {
                    v.push(s);
                }
            }
            Ok(v)
        };
        let net = match &g.net {
            NetGrant::Off => NetGrant::Off,
            NetGrant::All => NetGrant::All,
            NetGrant::Hosts(hosts) => {
                let mut v = Vec::new();
                for h in hosts {
                    let s = substitute(h, values)?;
                    if !v.contains(&s) {
                        v.push(s);
                    }
                }
                NetGrant::Hosts(v)
            }
        };
        let mut env = Vec::new();
        for e in &g.env {
            let s = substitute(e, values)?;
            if !env.contains(&s) {
                env.push(s);
            }
        }
        out.push((
            plugin.clone(),
            Grants {
                fs_read: paths(&g.fs_read)?,
                fs_write: paths(&g.fs_write)?,
                exec: g.exec.clone(),
                net,
                env,
                unknown: BTreeMap::new(),
            },
        ));
    }
    Ok(out)
}

/// Prüft die aufgelösten Werte auf Plausibilität — liefert Hinweise, keine Fehler: Ein
/// `kind = "path"`, der noch nicht existiert, ist oft gewollt (der Nutzer legt ihn danach an).
pub fn value_notes(
    manifest: &PkgManifest,
    values: &BTreeMap<String, String>,
    ctx: &ResolveCtx,
) -> Vec<String> {
    let mut notes = Vec::new();
    for (name, spec) in &manifest.vars {
        if spec.kind != VarKind::Path {
            continue;
        }
        if let Some(v) = values.get(name) {
            let abs = canonicalize_lenient(&resolve_path_with(v, ctx));
            if !abs.exists() {
                notes.push(format!(
                    "{name} = {} existiert noch nicht (wird nicht angelegt)",
                    abs.display()
                ));
            }
        }
    }
    notes
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::PkgManifest;

    fn manifest(rights: &str) -> PkgManifest {
        let key = "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=";
        PkgManifest::parse(&format!(
            "name = \"demo\"\nversion = \"1.0.0\"\n[publisher]\nname = \"acme\"\nkey = \"{key}\"\n\
             [vars.BELEGE_DIR]\ndescription = \"x\"\nkind = \"path\"\ndefault = \"~/belege\"\n\
             [vars.MANDANT]\ndescription = \"y\"\n\
             [files]\n\"plugins/p.wasm\" = \"{h}\"\n\"plugins/p.toml\" = \"{h}\"\n{rights}",
            h = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        ))
        .unwrap()
    }

    fn ctx(home: &str, cwd: &str) -> ResolveCtx {
        ResolveCtx {
            home: Some(home.into()),
            cwd: cwd.into(),
            tmpdir: "/tmp".into(),
        }
    }

    #[test]
    fn given_beats_previous_beats_default_and_reports_missing() {
        let m = manifest("");
        let r = resolve_vars(&m, &BTreeMap::new(), None).unwrap();
        assert_eq!(r.values["BELEGE_DIR"], "~/belege");
        assert_eq!(r.missing.len(), 1);
        assert_eq!(r.missing[0].0, "MANDANT");

        let prev = BTreeMap::from([("MANDANT".to_string(), "alt".to_string())]);
        let r = resolve_vars(&m, &BTreeMap::new(), Some(&prev)).unwrap();
        assert_eq!(r.values["MANDANT"], "alt");
        assert!(r.missing.is_empty());

        let given = BTreeMap::from([("MANDANT".to_string(), "neu".to_string())]);
        let r = resolve_vars(&m, &given, Some(&prev)).unwrap();
        assert_eq!(r.values["MANDANT"], "neu");

        let bogus = BTreeMap::from([("FOO".to_string(), "x".to_string())]);
        let e = resolve_vars(&m, &bogus, None).unwrap_err();
        assert!(e.to_string().contains("nicht deklariert"), "{e}");
    }

    #[test]
    fn substitute_replaces_every_placeholder_and_rejects_unknown() {
        let v = BTreeMap::from([("A".to_string(), "1".to_string())]);
        assert_eq!(substitute("x/${A}/${A}", &v).unwrap(), "x/1/1");
        assert_eq!(substitute("ohne", &v).unwrap(), "ohne");
        assert!(substitute("${B}", &v).is_err());
        assert!(substitute("${A", &v).is_err());
    }

    #[test]
    fn rights_become_absolute_paths_and_keep_hosts_verbatim() {
        let m = manifest(
            "[rights.p]\nfs_read = [\"${BELEGE_DIR}\", \"./rel\", \"${BELEGE_DIR}\"]\n\
             net = [\"api.${MANDANT}.example\"]\nenv = [\"TOKEN_${MANDANT}\"]\n",
        );
        let values = BTreeMap::from([
            ("BELEGE_DIR".to_string(), "~/belege".to_string()),
            ("MANDANT".to_string(), "acme".to_string()),
        ]);
        let out = resolve_rights(&m, &values, &ctx("/home/anna", "/work")).unwrap();
        assert_eq!(out.len(), 1);
        let (plugin, g) = &out[0];
        assert_eq!(plugin, "p");
        assert_eq!(
            g.fs_read,
            vec!["/home/anna/belege".to_string(), "/work/rel".to_string()]
        );
        assert_eq!(g.net, NetGrant::Hosts(vec!["api.acme.example".into()]));
        assert_eq!(g.env, vec!["TOKEN_acme".to_string()]);
        let notes = value_notes(&m, &values, &ctx("/home/anna", "/work"));
        assert_eq!(notes.len(), 1, "{notes:?}");
        assert!(notes[0].contains("/home/anna/belege"), "{notes:?}");
    }
}
