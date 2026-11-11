//! The Snippets tool: saved commands and prompts, a local take on Warp
//! Drive's workflows. They live in `snippets.toml` in the config directory,
//! which the panel opens in Sumi's editor for bulk edits.
//!
//! A `{{name}}` placeholder marks text to fill in: such a snippet is typed
//! into the prompt without Enter, so the user replaces the placeholder
//! before running it.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::time::SystemTime;

pub(crate) const FILE_NAME: &str = "snippets.toml";

const TEMPLATE: &str = r#"# Sumi snippets: saved commands and prompts shown in the left panel.
# Click a snippet to type it into the active pane; right-click to run it.
# Text in {{double braces}} is a placeholder to fill in before running.
#
# [[snippets]]
# name = "Recent commits"
# command = "git log --oneline -20"
#
# [[snippets]]
# name = "Find files"
# command = "fd {{pattern}}"
"#;

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Snippet {
    #[serde(default)]
    pub name: String,
    pub command: String,
}

impl Snippet {
    pub fn label(&self) -> &str {
        if self.name.trim().is_empty() {
            self.command.lines().next().unwrap_or_default()
        } else {
            &self.name
        }
    }

    pub fn has_placeholder(&self) -> bool {
        let Some(open) = self.command.find("{{") else {
            return false;
        };
        self.command[open..].contains("}}")
    }
}

#[derive(Default, Serialize, Deserialize)]
struct SnippetFile {
    #[serde(default)]
    snippets: Vec<Snippet>,
}

#[derive(Default)]
pub(crate) struct SnippetStore {
    pub snippets: Vec<Snippet>,
    pub error: Option<String>,
    loaded_mtime: Option<SystemTime>,
    loaded: bool,
}

pub(crate) fn snippets_path() -> PathBuf {
    config::CONFIG_DIRS
        .first()
        .cloned()
        .unwrap_or_else(|| config::HOME_DIR.join(".config").join("sumi"))
        .join(FILE_NAME)
}

pub(crate) fn parse(text: &str) -> Result<Vec<Snippet>, String> {
    toml::from_str::<SnippetFile>(text)
        .map(|file| {
            file.snippets
                .into_iter()
                .filter(|s| !s.command.trim().is_empty())
                .collect()
        })
        .map_err(|err| err.to_string())
}

pub(crate) fn render(snippets: &[Snippet]) -> String {
    let body = toml::to_string_pretty(&SnippetFile {
        snippets: snippets.to_vec(),
    })
    .unwrap_or_default();
    format!("{TEMPLATE}\n{body}")
}

/// The name a new snippet gets: the command's first word or two.
pub(crate) fn default_name(command: &str) -> String {
    let words: Vec<&str> = command.split_whitespace().take(3).collect();
    words.join(" ")
}

impl SnippetStore {
    /// Reloads when the file changed on disk since the last read.
    pub fn reload_if_changed(&mut self) -> bool {
        let path = snippets_path();
        let mtime = std::fs::metadata(&path).and_then(|m| m.modified()).ok();
        if self.loaded && mtime == self.loaded_mtime {
            return false;
        }
        self.loaded = true;
        self.loaded_mtime = mtime;
        let before = (self.snippets.clone(), self.error.clone());
        match std::fs::read_to_string(&path) {
            Ok(text) => match parse(&text) {
                Ok(snippets) => {
                    self.snippets = snippets;
                    self.error = None;
                }
                Err(err) => self.error = Some(err),
            },
            Err(_) => {
                self.snippets.clear();
                self.error = None;
            }
        }
        before != (self.snippets.clone(), self.error.clone())
    }

    pub fn save(&mut self) -> anyhow::Result<()> {
        let path = snippets_path();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let tmp = path.with_extension(format!("toml.{}.tmp", std::process::id()));
        std::fs::write(&tmp, render(&self.snippets))?;
        std::fs::rename(&tmp, &path)?;
        self.loaded_mtime = std::fs::metadata(&path).and_then(|m| m.modified()).ok();
        Ok(())
    }

    /// Makes sure the file exists, so the editor opens on the template.
    pub fn ensure_file(&mut self) -> anyhow::Result<PathBuf> {
        let path = snippets_path();
        if !path.exists() {
            self.save()?;
        }
        Ok(path)
    }

    pub fn add(&mut self, command: String) -> anyhow::Result<()> {
        self.reload_if_changed();
        if self.error.is_some() {
            anyhow::bail!("snippets.toml has errors; fix it before adding");
        }
        self.snippets.push(Snippet {
            name: default_name(&command),
            command,
        });
        self.save()
    }

    pub fn remove(&mut self, idx: usize) -> anyhow::Result<()> {
        self.reload_if_changed();
        if idx < self.snippets.len() && self.error.is_none() {
            self.snippets.remove(idx);
            self.save()?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rendered_file_parses_back_and_keeps_the_template_comments() {
        let snippets = vec![
            Snippet {
                name: "Log".into(),
                command: "git log --oneline".into(),
            },
            Snippet {
                name: String::new(),
                command: "echo \"多行\"\nls".into(),
            },
        ];
        let text = render(&snippets);
        assert!(text.starts_with("# Sumi snippets"));
        assert_eq!(parse(&text).unwrap(), snippets);
        assert_eq!(snippets[1].label(), "echo \"多行\"");
        // The commented examples in the template are not snippets.
        assert_eq!(parse(TEMPLATE).unwrap(), vec![]);
    }

    #[test]
    fn placeholders_and_names() {
        let s = |c: &str| Snippet {
            name: String::new(),
            command: c.into(),
        };
        assert!(s("fd {{pattern}}").has_placeholder());
        assert!(!s("echo {{").has_placeholder());
        assert!(!s("echo }} {{").has_placeholder());
        assert_eq!(default_name("git log --oneline -20"), "git log --oneline");
        assert!(parse("[[snippets]]\nname = 1").is_err());
    }
}
