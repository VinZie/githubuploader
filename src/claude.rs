//! Asking Claude Code (`claude -p`) to work inside a project folder, with
//! its progress streamed into the activity log.

use std::path::{Path, PathBuf};
use std::time::Instant;

use serde_json::Value;

use crate::cmd::Cmd;
use crate::jobs::{Job, LogKind};

/// Looking around only.
pub const READ_ONLY: &[&str] = &["Read", "Glob", "Grep"];
/// Looking around and editing files.
pub const EDIT: &[&str] = &["Read", "Glob", "Grep", "Edit", "Write"];
/// Added when the user lets Claude run git and gh itself.
pub const GIT: &[&str] = &["Bash(git:*)", "Bash(gh:*)"];

/// The Claude Code executable: the native installer puts it in
/// ~/.local/bin, which a GUI app's PATH may not include.
pub fn exe() -> PathBuf {
    std::env::var_os("USERPROFILE")
        .map(|home| PathBuf::from(home).join(".local").join("bin").join("claude.exe"))
        .filter(|path| path.is_file())
        .unwrap_or_else(|| "claude".into())
}

pub struct Ask<'a> {
    pub dir: &'a Path,
    pub prompt: String,
    /// Model alias; empty for Claude Code's default.
    pub model: &'a str,
    pub tools: Vec<&'a str>,
}

/// Runs Claude to completion and returns its final answer.
pub fn ask(job: &Job, ask: Ask) -> Result<String, String> {
    let started = Instant::now();
    let mut cmd = Cmd::new(exe())
        .cwd(ask.dir)
        .args(["-p", "--output-format", "stream-json", "--verbose"])
        // Only the tools allowed below, whatever the user's own default mode is.
        .args(["--permission-mode", "default"]);
    if !ask.model.is_empty() {
        cmd = cmd.args(["--model", ask.model]);
    }
    if !ask.tools.is_empty() {
        cmd = cmd.args(["--allowedTools", &ask.tools.join(",")]);
    }
    let cmd = cmd.stdin(ask.prompt);

    job.log(LogKind::Claude, "Claude is working…");
    let mut result: Option<(bool, String)> = None;
    let mut cost = None;
    let out = cmd.run_lines(Some(job), |line| {
        let Ok(event) = serde_json::from_str::<Value>(line) else {
            if !line.trim().is_empty() {
                job.log(LogKind::Out, line);
            }
            return;
        };
        match event["type"].as_str() {
            Some("assistant") => {
                for block in event["message"]["content"].as_array().into_iter().flatten() {
                    match block["type"].as_str() {
                        Some("text") => job.log(LogKind::Claude, block["text"].as_str().unwrap_or_default()),
                        Some("tool_use") => job.log(LogKind::Tool, describe_tool(block, ask.dir)),
                        _ => {}
                    }
                }
            }
            Some("result") => {
                for denial in event["permission_denials"].as_array().into_iter().flatten() {
                    job.warn(format!(
                        "Claude wasn't allowed to use {}",
                        denial["tool_name"].as_str().unwrap_or("a tool")
                    ));
                }
                cost = event["total_cost_usd"].as_f64();
                let is_error = event["is_error"].as_bool().unwrap_or(false);
                result = Some((is_error, event["result"].as_str().unwrap_or_default().to_string()));
            }
            _ => {}
        }
    })?;

    let seconds = started.elapsed().as_secs();
    match result {
        Some((false, text)) => {
            let cost = cost.map(|c| format!(" · ${c:.3}")).unwrap_or_default();
            job.info(format!("Claude finished in {seconds}s{cost}"));
            Ok(text)
        }
        Some((true, text)) => Err(format!("Claude stopped with an error: {text}")),
        None if !out.ok => Err(format!("Claude Code failed: {}", out.err_text())),
        None => Err("Claude Code ended without an answer.".into()),
    }
}

/// "Read src/main.rs", "Bash git status", … for the activity log.
fn describe_tool(block: &Value, dir: &Path) -> String {
    let name = block["name"].as_str().unwrap_or("tool");
    let input = &block["input"];
    let detail = ["file_path", "path", "pattern", "command", "description"]
        .iter()
        .find_map(|key| input[*key].as_str())
        .or_else(|| input.as_object()?.values().find_map(Value::as_str))
        .unwrap_or_default();
    let dir = dir.to_string_lossy();
    let detail = detail
        .strip_prefix(dir.as_ref())
        .map(|rest| rest.trim_start_matches(['\\', '/']))
        .unwrap_or(detail);
    let mut detail: String = detail.lines().next().unwrap_or_default().chars().take(140).collect();
    if detail.is_empty() {
        detail = "…".into();
    }
    format!("→ {name} {detail}")
}

/// The last JSON object in Claude's answer, which prompts ask it to end with.
pub fn last_json(text: &str) -> Option<Value> {
    let end = text.rfind('}')?;
    text[..end]
        .rmatch_indices('{')
        .find_map(|(start, _)| serde_json::from_str::<Value>(&text[start..=end]).ok())
        .filter(Value::is_object)
}

/// Claude's answer without Markdown code fences around it.
pub fn unfence(text: &str) -> String {
    let text = text.trim();
    let Some(inner) = text.strip_prefix("```") else {
        return text.to_string();
    };
    let inner = inner.split_once('\n').map_or("", |(_, rest)| rest);
    inner.trim_end().trim_end_matches("```").trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_trailing_json() {
        let answer = "I updated .gitignore.\n{\"description\": \"A tool\", \"topics\": [\"rust\"]}";
        let json = last_json(answer).unwrap();
        assert_eq!(json["description"], "A tool");
        assert_eq!(last_json("no json here"), None);
    }

    #[test]
    fn strips_fences() {
        assert_eq!(unfence("```text\nFix bug\n```"), "Fix bug");
        assert_eq!(unfence("Fix bug"), "Fix bug");
    }
}
