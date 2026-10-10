//! App preferences, kept in %APPDATA%\githubuploader\config.json.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

#[derive(Clone, Serialize, Deserialize)]
pub struct SavedPrompt {
    pub name: String,
    pub text: String,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// The folder whose subfolders are the projects.
    pub root: String,
    /// Visibility new repositories start out with.
    pub default_private: bool,
    /// Claude model alias; empty means Claude Code's own default.
    pub model: String,
    /// License key added to new repositories; empty for none.
    pub default_license: String,
    /// GitHub usernames invited to every new repository.
    pub default_collaborators: Vec<String>,
    /// Whether uploads of new repositories let Claude tidy them up first.
    pub claude_prepares: bool,
    /// The user's own Claude prompts.
    pub prompts: Vec<SavedPrompt>,
}

impl Default for Config {
    fn default() -> Self {
        let root = std::env::var_os("USERPROFILE")
            .map(|home| PathBuf::from(home).join("Desktop").join("claude-stuff"))
            .filter(|dir| dir.is_dir())
            .or_else(|| std::env::current_dir().ok())
            .unwrap_or_default();
        Self {
            root: root.to_string_lossy().into_owned(),
            default_private: true,
            model: String::new(),
            default_license: String::new(),
            default_collaborators: Vec::new(),
            claude_prepares: true,
            prompts: Vec::new(),
        }
    }
}

fn path() -> Option<PathBuf> {
    std::env::var_os("APPDATA").map(|dir| PathBuf::from(dir).join("githubuploader").join("config.json"))
}

impl Config {
    pub fn load() -> Self {
        path()
            .and_then(|path| std::fs::read_to_string(path).ok())
            .and_then(|text| serde_json::from_str(&text).ok())
            .unwrap_or_default()
    }

    pub fn save(&self) {
        let Some(path) = path() else { return };
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        if let Ok(text) = serde_json::to_string_pretty(self) {
            let _ = std::fs::write(path, text);
        }
    }
}
