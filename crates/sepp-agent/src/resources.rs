//! Tier 0 — Resources (reine Daten): Skills, Prompt-Templates, Themes.
//!
//! Discovery aus konventionellen Wurzeln (`docs/05-extensibility-tiers.md`):
//! `<root>/skills/`, `<root>/prompts/`, `<root>/themes/`. Üblich: global `~/.sepp` und
//! projektlokal `<repo>/.sepp` (letzteres erst nach Trust laden). Skills fließen in den
//! System-Prompt, Prompt-Templates werden zu Slash-Commands. Resources sind inert (keine
//! Capabilities).

use std::path::{Path, PathBuf};

/// Eine Fähigkeit (`SKILL.md` oder Verzeichnis mit `SKILL.md`).
#[derive(Debug, Clone)]
pub struct Skill {
    pub name: String,
    pub content: String,
}

/// Ein Prompt-Template (`<name>.md`) → Slash-Command `/<name>`.
#[derive(Debug, Clone)]
pub struct PromptTemplate {
    pub name: String,
    pub content: String,
}

/// Ein Theme (`<name>.toml`).
#[derive(Debug, Clone)]
pub struct Theme {
    pub name: String,
    pub content: String,
}

/// Gesammelte Resources.
#[derive(Debug, Clone, Default)]
pub struct ResourceSet {
    pub skills: Vec<Skill>,
    pub prompts: Vec<PromptTemplate>,
    pub themes: Vec<Theme>,
}

impl ResourceSet {
    /// Lädt Resources aus mehreren Wurzeln in Reihenfolge (z. B. global, dann projektlokal).
    pub fn load(roots: &[PathBuf]) -> Self {
        let mut set = ResourceSet::default();
        for root in roots {
            set.skills.extend(load_skills(&root.join("skills")));
            set.prompts.extend(load_prompts(&root.join("prompts")));
            set.themes.extend(load_themes(&root.join("themes")));
        }
        set
    }

    /// Skill-Inhalte als System-Prompt-Ergänzung (leer, wenn keine Skills vorhanden sind).
    pub fn system_prompt_addition(&self) -> String {
        if self.skills.is_empty() {
            return String::new();
        }
        let mut s = String::from("\n\n# Verfügbare Skills\n");
        for sk in &self.skills {
            s.push_str(&format!("\n## {}\n{}\n", sk.name, sk.content.trim()));
        }
        s
    }

    /// Findet ein Prompt-Template per Name.
    pub fn prompt(&self, name: &str) -> Option<&PromptTemplate> {
        self.prompts.iter().find(|p| p.name == name)
    }
}

fn read(path: &Path) -> Option<String> {
    std::fs::read_to_string(path).ok()
}

fn stem(path: &Path) -> Option<String> {
    path.file_stem().and_then(|n| n.to_str()).map(String::from)
}

fn has_ext(path: &Path, ext: &str) -> bool {
    path.extension().and_then(|x| x.to_str()) == Some(ext)
}

fn load_skills(dir: &Path) -> Vec<Skill> {
    let mut out = Vec::new();
    let Ok(rd) = std::fs::read_dir(dir) else {
        return out;
    };
    for e in rd.flatten() {
        let path = e.path();
        if path.is_dir() {
            // Verzeichnis mit SKILL.md → Skill-Name = Verzeichnisname.
            if let Some(content) = read(&path.join("SKILL.md")) {
                if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
                    out.push(Skill {
                        name: name.to_string(),
                        content,
                    });
                }
            }
        } else if has_ext(&path, "md") {
            if let (Some(name), Some(content)) = (stem(&path), read(&path)) {
                out.push(Skill { name, content });
            }
        }
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

fn load_prompts(dir: &Path) -> Vec<PromptTemplate> {
    let mut out = Vec::new();
    let Ok(rd) = std::fs::read_dir(dir) else {
        return out;
    };
    for e in rd.flatten() {
        let path = e.path();
        if has_ext(&path, "md") {
            if let (Some(name), Some(content)) = (stem(&path), read(&path)) {
                out.push(PromptTemplate { name, content });
            }
        }
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

fn load_themes(dir: &Path) -> Vec<Theme> {
    let mut out = Vec::new();
    let Ok(rd) = std::fs::read_dir(dir) else {
        return out;
    };
    for e in rd.flatten() {
        let path = e.path();
        if has_ext(&path, "toml") {
            if let (Some(name), Some(content)) = (stem(&path), read(&path)) {
                out.push(Theme { name, content });
            }
        }
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loads_skills_prompts_and_builds_system_prompt() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().to_path_buf();

        // skills/recherche/SKILL.md
        std::fs::create_dir_all(root.join("skills/recherche")).unwrap();
        std::fs::write(
            root.join("skills/recherche/SKILL.md"),
            "Nutze Quellen sorgfältig.",
        )
        .unwrap();
        // skills/kurz.md (Einzeldatei)
        std::fs::write(root.join("skills/kurz.md"), "Antworte knapp.").unwrap();
        // prompts/review.md
        std::fs::create_dir_all(root.join("prompts")).unwrap();
        std::fs::write(root.join("prompts/review.md"), "Review bitte: ").unwrap();

        let set = ResourceSet::load(&[root]);
        assert_eq!(set.skills.len(), 2);
        assert!(set.skills.iter().any(|s| s.name == "recherche"));
        assert!(set.skills.iter().any(|s| s.name == "kurz"));

        let sp = set.system_prompt_addition();
        assert!(sp.contains("Verfügbare Skills"));
        assert!(sp.contains("Nutze Quellen sorgfältig."));
        assert!(sp.contains("Antworte knapp."));

        assert!(set.prompt("review").is_some());
        assert!(set
            .prompt("review")
            .unwrap()
            .content
            .contains("Review bitte"));
        assert!(set.prompt("fehlt").is_none());
    }

    #[test]
    fn missing_dirs_are_ok() {
        let tmp = tempfile::tempdir().unwrap();
        let set = ResourceSet::load(&[tmp.path().to_path_buf()]);
        assert!(set.skills.is_empty());
        assert!(set.system_prompt_addition().is_empty());
    }
}
