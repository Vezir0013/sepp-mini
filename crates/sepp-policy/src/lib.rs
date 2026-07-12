//! `sepp-policy` — Capabilities, Policy-Auswertung und OS-Sandbox.
//!
//! Default ist **deny**: was nicht deklariert ist, gibt es nicht.
//! Eine [`Policy`] ist die Menge gewährter [`Capability`]s, gelesen aus einem [`Manifest`].
//! Durchsetzung out-of-process über [`Sandbox`] (Linux: Landlock, macOS: Seatbelt; sonst
//! portabler Fallback ohne Durchsetzung + Warnung).

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use sepp_core::{Result, SeppError};

mod sandbox;
mod secrets;
#[cfg(target_os = "linux")]
pub use sandbox::LandlockSandbox;
pub use sandbox::{default_sandbox, NullSandbox, Sandbox};
pub use secrets::SecretBroker;

/// Ein atomares Recht.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Capability {
    FsRead {
        prefix: PathBuf,
    },
    FsWrite {
        prefix: PathBuf,
    },
    /// Host exakt (`api.example.com`) oder Glob (`*.example.com`).
    Net {
        host: String,
    },
    Env {
        name: String,
    },
    Exec {
        program: String,
    },
}

/// Die Menge gewährter Capabilities.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Policy {
    pub granted: Vec<Capability>,
}

impl Policy {
    pub fn new(granted: Vec<Capability>) -> Self {
        Policy { granted }
    }

    /// Erlaubt die Policy das angefragte Recht? Default deny.
    pub fn allows(&self, requested: &Capability) -> bool {
        self.granted.iter().any(|g| covers(g, requested))
    }

    /// Lese-Pfad-Präfixe (für die Sandbox).
    pub fn fs_read_prefixes(&self) -> Vec<PathBuf> {
        self.granted
            .iter()
            .filter_map(|c| match c {
                Capability::FsRead { prefix } => Some(prefix.clone()),
                _ => None,
            })
            .collect()
    }

    /// Schreib-Pfad-Präfixe (für die Sandbox).
    pub fn fs_write_prefixes(&self) -> Vec<PathBuf> {
        self.granted
            .iter()
            .filter_map(|c| match c {
                Capability::FsWrite { prefix } => Some(prefix.clone()),
                _ => None,
            })
            .collect()
    }
}

/// Deckt das gewährte Recht `g` das angefragte `r`?
fn covers(g: &Capability, r: &Capability) -> bool {
    use Capability::*;
    match (g, r) {
        (FsRead { prefix: gp }, FsRead { prefix: rp }) => rp.starts_with(gp),
        (FsWrite { prefix: gp }, FsWrite { prefix: rp }) => rp.starts_with(gp),
        (Net { host: gh }, Net { host: rh }) => host_matches(gh, rh),
        (Env { name: gn }, Env { name: rn }) => gn == rn,
        (Exec { program: gp }, Exec { program: rp }) => gp == rp,
        _ => false,
    }
}

/// `*.example.com` matcht Subdomains (`api.example.com`), nicht den Apex; sonst exakt.
fn host_matches(pattern: &str, host: &str) -> bool {
    match pattern.strip_prefix("*.") {
        Some(suffix) => host.len() > suffix.len() + 1 && host.ends_with(&format!(".{suffix}")),
        None => pattern == host,
    }
}

/// Manifest einer code-führenden Erweiterung (`manifest.toml`).
#[derive(Debug, Clone, Deserialize)]
pub struct Manifest {
    pub name: String,
    #[serde(default)]
    pub version: Option<String>,
    /// `"wasm"` | `"mcp"`.
    #[serde(default)]
    pub kind: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    /// Nur `kind = "wasm"`.
    #[serde(default)]
    pub entry: Option<String>,
    #[serde(default)]
    pub capabilities: Capabilities,
    /// Ressourcen-Limits (`[limits]`; fehlend = konservative Defaults, nie „unbegrenzt").
    #[serde(default)]
    pub limits: Limits,
}

/// Deklarierte Capabilities (Manifest- bzw. `[mcp.servers.capabilities]`-Form).
#[derive(Debug, Clone, Default, Deserialize)]
pub struct Capabilities {
    #[serde(default)]
    pub fs_read: Vec<String>,
    #[serde(default)]
    pub fs_write: Vec<String>,
    #[serde(default)]
    pub net: Vec<String>,
    #[serde(default)]
    pub env: Vec<String>,
    #[serde(default)]
    pub exec: Vec<String>,
}

impl Capabilities {
    /// Baut eine [`Policy`]; Pfade werden (`~`/relativ aufgelöst und) kanonisiert.
    pub fn to_policy(&self) -> Policy {
        let mut granted = Vec::new();
        for p in &self.fs_read {
            granted.push(Capability::FsRead {
                prefix: resolve_path(p),
            });
        }
        for p in &self.fs_write {
            granted.push(Capability::FsWrite {
                prefix: resolve_path(p),
            });
        }
        for h in &self.net {
            granted.push(Capability::Net { host: h.clone() });
        }
        for e in &self.env {
            granted.push(Capability::Env { name: e.clone() });
        }
        for x in &self.exec {
            granted.push(Capability::Exec { program: x.clone() });
        }
        Policy { granted }
    }
}

/// Ressourcen-Limits einer code-führenden Erweiterung (`[limits]` im Manifest).
///
/// Konzeptuell dieselbe Logik wie [`Capability`], nur für **Verbrauch** (CPU, Speicher, Wanduhr)
/// statt für Zugriff: kein deklariertes Limit heißt konservativer Default, nicht „unbegrenzt".
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct Limits {
    /// Maximale lineare WASM-Speichergröße in 64-KiB-Pages (Default: 256 = 16 MiB).
    pub max_memory_pages: u32,
    /// Wanduhr-Budget eines Tool-Aufrufs in Millisekunden. `0` heißt: beliebig lange laufen
    /// dürfen — aber weiterhin an jedem Yield-Punkt unterbrechbar (Default: 30 000).
    pub max_wall_time_ms: u64,
    /// Instruktionen pro Zeitscheibe — das Yield-Intervall des Fuel-Slicings (Default: 1 000 000).
    pub fuel_slice: u64,
}

impl Default for Limits {
    fn default() -> Self {
        Limits {
            max_memory_pages: 256,
            max_wall_time_ms: 30_000,
            fuel_slice: 1_000_000,
        }
    }
}

impl Limits {
    /// Speicherlimit in Bytes (Pages × 64 KiB).
    pub fn max_memory_bytes(&self) -> usize {
        self.max_memory_pages as usize * 64 * 1024
    }

    /// Weist unplausible Werte zurück — lieber gar nicht laden als unkontrolliert laufen.
    pub fn validate(&self) -> Result<()> {
        if self.fuel_slice == 0 {
            return Err(SeppError::Config(
                "manifest [limits]: fuel_slice muss > 0 sein".into(),
            ));
        }
        if self.max_memory_pages == 0 || self.max_memory_pages > 65_536 {
            return Err(SeppError::Config(format!(
                "manifest [limits]: max_memory_pages muss in 1..=65536 liegen (ist {})",
                self.max_memory_pages
            )));
        }
        Ok(())
    }
}

impl Manifest {
    pub fn parse(toml_str: &str) -> Result<Manifest> {
        let m: Manifest =
            toml::from_str(toml_str).map_err(|e| SeppError::Config(format!("manifest: {e}")))?;
        m.limits.validate()?;
        Ok(m)
    }

    pub fn from_file(path: &Path) -> Result<Manifest> {
        let text = std::fs::read_to_string(path)
            .map_err(|e| SeppError::Config(format!("manifest {}: {e}", path.display())))?;
        Self::parse(&text)
    }

    pub fn policy(&self) -> Policy {
        self.capabilities.to_policy()
    }
}

/// Löst `~` (Home) und relative `./`-Pfade (gegen cwd) auf und kanonisiert best-effort.
fn resolve_path(p: &str) -> PathBuf {
    let expanded: PathBuf = if let Some(rest) = p.strip_prefix('~') {
        match std::env::var_os("HOME") {
            Some(home) => PathBuf::from(home).join(rest.trim_start_matches('/')),
            None => PathBuf::from(p),
        }
    } else {
        PathBuf::from(p)
    };
    let abs = if expanded.is_absolute() {
        expanded
    } else {
        std::env::current_dir()
            .map(|c| c.join(&expanded))
            .unwrap_or(expanded)
    };
    abs.canonicalize().unwrap_or(abs)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(s: &str) -> PathBuf {
        PathBuf::from(s)
    }

    #[test]
    fn fs_prefix_allows_subpaths_only() {
        let pol = Policy::new(vec![Capability::FsWrite {
            prefix: p("/proj/out"),
        }]);
        assert!(pol.allows(&Capability::FsWrite {
            prefix: p("/proj/out/sub/x")
        }));
        assert!(pol.allows(&Capability::FsWrite {
            prefix: p("/proj/out")
        }));
        assert!(!pol.allows(&Capability::FsWrite {
            prefix: p("/proj/other")
        }));
        // FsWrite-Grant ist kein FsRead-Grant.
        assert!(!pol.allows(&Capability::FsRead {
            prefix: p("/proj/out/x")
        }));
    }

    #[test]
    fn net_glob_matches_subdomains_not_apex() {
        let pol = Policy::new(vec![
            Capability::Net {
                host: "*.example.com".into(),
            },
            Capability::Net {
                host: "api.foo.com".into(),
            },
        ]);
        assert!(pol.allows(&Capability::Net {
            host: "a.example.com".into()
        }));
        assert!(pol.allows(&Capability::Net {
            host: "x.y.example.com".into()
        }));
        assert!(!pol.allows(&Capability::Net {
            host: "example.com".into()
        }));
        assert!(pol.allows(&Capability::Net {
            host: "api.foo.com".into()
        }));
        assert!(!pol.allows(&Capability::Net {
            host: "evil.com".into()
        }));
    }

    #[test]
    fn env_and_exec_exact() {
        let pol = Policy::new(vec![
            Capability::Env {
                name: "TOKEN".into(),
            },
            Capability::Exec {
                program: "git".into(),
            },
        ]);
        assert!(pol.allows(&Capability::Env {
            name: "TOKEN".into()
        }));
        assert!(!pol.allows(&Capability::Env {
            name: "OTHER".into()
        }));
        assert!(pol.allows(&Capability::Exec {
            program: "git".into()
        }));
        assert!(!pol.allows(&Capability::Exec {
            program: "rm".into()
        }));
    }

    #[test]
    fn empty_policy_denies_everything() {
        let pol = Policy::default();
        assert!(!pol.allows(&Capability::Net {
            host: "x.com".into()
        }));
        assert!(!pol.allows(&Capability::FsRead { prefix: p("/") }));
    }

    #[test]
    fn manifest_parses_capabilities() {
        let toml = r#"
            name = "string-tools"
            version = "0.1.0"
            kind = "wasm"
            entry = "string_tools.wasm"

            [capabilities]
            fs_read = ["/abs/read"]
            net = ["api.example.com", "*.cdn.example.com"]
            env = ["LEVEL"]
        "#;
        let m = Manifest::parse(toml).unwrap();
        assert_eq!(m.name, "string-tools");
        assert_eq!(m.kind.as_deref(), Some("wasm"));
        let pol = m.policy();
        assert!(pol.allows(&Capability::FsRead {
            prefix: p("/abs/read/x")
        }));
        assert!(pol.allows(&Capability::Net {
            host: "sub.cdn.example.com".into()
        }));
        assert!(pol.allows(&Capability::Env {
            name: "LEVEL".into()
        }));
        assert!(!pol.allows(&Capability::Net {
            host: "evil.com".into()
        }));
    }

    #[test]
    fn manifest_without_capabilities_is_pure() {
        let m = Manifest::parse("name=\"reverse\"\nkind=\"wasm\"\nentry=\"r.wasm\"").unwrap();
        assert!(m.policy().granted.is_empty());
    }

    #[test]
    fn manifest_without_limits_uses_conservative_defaults() {
        let m = Manifest::parse("name=\"reverse\"\nkind=\"wasm\"\nentry=\"r.wasm\"").unwrap();
        assert_eq!(m.limits, Limits::default());
        assert_eq!(m.limits.max_memory_pages, 256);
        assert_eq!(m.limits.max_wall_time_ms, 30_000);
        assert_eq!(m.limits.fuel_slice, 1_000_000);
        assert_eq!(m.limits.max_memory_bytes(), 16 * 1024 * 1024);
    }

    #[test]
    fn manifest_parses_limits_section() {
        let toml = r#"
            name = "cruncher"
            kind = "wasm"
            entry = "c.wasm"

            [limits]
            max_memory_pages = 512
            max_wall_time_ms = 0
            fuel_slice = 50000
        "#;
        let m = Manifest::parse(toml).unwrap();
        assert_eq!(m.limits.max_memory_pages, 512);
        // 0 = unbegrenzt lange laufen dürfen (explizit erlaubt, aber unterbrechbar).
        assert_eq!(m.limits.max_wall_time_ms, 0);
        assert_eq!(m.limits.fuel_slice, 50_000);
    }

    #[test]
    fn manifest_rejects_implausible_limits() {
        let zero_fuel = "name=\"x\"\n[limits]\nfuel_slice = 0";
        assert!(
            Manifest::parse(zero_fuel).is_err(),
            "fuel_slice=0 muss scheitern"
        );

        let huge_mem = "name=\"x\"\n[limits]\nmax_memory_pages = 100000";
        assert!(
            Manifest::parse(huge_mem).is_err(),
            "max_memory_pages>65536 muss scheitern"
        );

        let zero_mem = "name=\"x\"\n[limits]\nmax_memory_pages = 0";
        assert!(
            Manifest::parse(zero_mem).is_err(),
            "max_memory_pages=0 muss scheitern"
        );
    }
}
