//! Project folders: what state each one is in, and the pre-flight scan of
//! what an upload would publish.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::cmd::git;
use crate::jobs::Job;

#[derive(Clone)]
pub struct ProjectInfo {
    pub name: String,
    pub path: PathBuf,
    pub is_git: bool,
    pub has_commits: bool,
    pub branch: Option<String>,
    pub origin: Option<String>,
    /// Owner and repository name, when `origin` points at GitHub.
    pub github: Option<(String, String)>,
    /// Uncommitted changes (`git status` entries).
    pub changes: usize,
    pub has_readme: bool,
    pub has_gitignore: bool,
    pub has_license: bool,
    pub empty: bool,
}

impl ProjectInfo {
    pub fn slug(&self) -> Option<String> {
        self.github.as_ref().map(|(owner, name)| format!("{owner}/{name}"))
    }
}

/// The project folders under `root`, sorted by name.
pub fn list(root: &Path) -> Result<Vec<PathBuf>, String> {
    let entries = std::fs::read_dir(root).map_err(|err| format!("can't read {}: {err}", root.display()))?;
    let mut dirs: Vec<PathBuf> = entries
        .flatten()
        .filter(|entry| entry.file_type().is_ok_and(|t| t.is_dir()))
        .filter(|entry| !entry.file_name().to_string_lossy().starts_with('.'))
        .map(|entry| entry.path())
        .collect();
    dirs.sort_by_key(|dir| dir.file_name().map(|n| n.to_string_lossy().to_lowercase()));
    Ok(dirs)
}

pub fn inspect(path: &Path) -> ProjectInfo {
    let names: Vec<String> = std::fs::read_dir(path)
        .map(|entries| {
            entries
                .flatten()
                .map(|entry| entry.file_name().to_string_lossy().to_lowercase())
                .collect()
        })
        .unwrap_or_default();
    let has = |prefixes: &[&str]| names.iter().any(|n| prefixes.iter().any(|p| n.starts_with(p)));
    let is_git = path.join(".git").exists();
    let mut info = ProjectInfo {
        name: path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default(),
        path: path.to_path_buf(),
        is_git,
        has_commits: false,
        branch: None,
        origin: None,
        github: None,
        changes: 0,
        has_readme: has(&["readme"]),
        has_gitignore: names.iter().any(|n| n == ".gitignore"),
        has_license: has(&["license", "licence", "copying"]),
        empty: names.is_empty(),
    };
    if is_git {
        let output = |args: &[&str]| {
            git(path)
                .args(args)
                .run(None)
                .ok()
                .filter(|out| out.ok)
                .map(|out| out.text().to_string())
        };
        info.has_commits = output(&["rev-parse", "--verify", "-q", "HEAD"]).is_some();
        info.branch = output(&["branch", "--show-current"]).filter(|b| !b.is_empty());
        info.origin = output(&["remote", "get-url", "origin"]).filter(|o| !o.is_empty());
        info.github = info.origin.as_deref().and_then(parse_github);
        info.changes = output(&["status", "--porcelain"]).map_or(0, |s| s.lines().count());
    }
    info
}

/// `("owner", "repo")` from an https or ssh GitHub remote URL.
pub fn parse_github(url: &str) -> Option<(String, String)> {
    let (_, rest) = url.split_once("github.com")?;
    let rest = rest.strip_prefix(['/', ':'])?;
    let rest = rest.trim_end_matches('/');
    let rest = rest.strip_suffix(".git").unwrap_or(rest);
    let (owner, name) = rest.split_once('/')?;
    (!owner.is_empty() && !name.is_empty()).then(|| (owner.to_string(), name.to_string()))
}

/// A folder name turned into something GitHub accepts as a repo name.
pub fn repo_name(folder: &str) -> String {
    let mut name = String::new();
    for c in folder.chars() {
        let c = if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.') { c } else { '-' };
        if !(c == '-' && name.ends_with('-')) {
            name.push(c);
        }
    }
    let name = name.trim_matches(['-', '.']);
    if name.is_empty() { "project".into() } else { name.into() }
}

/// GitHub topics: lowercase letters, digits and hyphens, at most 50 chars.
pub fn topics(text: &str) -> Vec<String> {
    let mut topics: Vec<String> = Vec::new();
    for raw in text.split([',', ' ', '\n']) {
        let mut topic: String = raw
            .trim()
            .to_lowercase()
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
            .collect();
        topic = topic.trim_matches('-').to_string();
        topic.truncate(50);
        if !topic.is_empty() && !topics.contains(&topic) {
            topics.push(topic);
        }
    }
    topics.truncate(20);
    topics
}

const MB: u64 = 1024 * 1024;
/// GitHub rejects pushes containing files over this size.
pub const HARD_LIMIT: u64 = 100 * MB;
/// GitHub warns about files over this size.
const SOFT_LIMIT: u64 = 50 * MB;

/// Folders that are almost always generated and don't belong in a repo.
const HEAVY_DIRS: &[&str] = &[
    "node_modules",
    "target",
    ".venv",
    "venv",
    "__pycache__",
    ".pytest_cache",
    ".mypy_cache",
    ".ruff_cache",
    ".next",
    ".nuxt",
    ".turbo",
    ".parcel-cache",
    ".gradle",
    ".vs",
    "dist",
    "build",
];

/// What an upload would publish, and anything in it that looks wrong.
#[derive(Clone, Default)]
pub struct ScanReport {
    /// Listed with `git ls-files` rather than by applying .gitignore rules.
    pub via_git: bool,
    pub files: usize,
    pub bytes: u64,
    pub too_big: Vec<(String, u64)>,
    pub large: Vec<(String, u64)>,
    pub secret_files: Vec<String>,
    pub secret_hits: Vec<(String, &'static str)>,
    pub heavy_dirs: Vec<(String, usize, u64)>,
    /// Files and bytes per top-level entry.
    pub top_level: Vec<(String, usize, u64)>,
    pub error: Option<String>,
}

impl ScanReport {
    pub fn blocked(&self) -> bool {
        self.error.is_some() || !self.too_big.is_empty()
    }

    pub fn warnings(&self) -> usize {
        self.large.len() + self.secret_files.len() + self.secret_hits.len() + self.heavy_dirs.len()
    }

    /// One line per problem, for prompts and logs.
    pub fn problems(&self) -> Vec<String> {
        let mut lines = Vec::new();
        for (path, size) in &self.too_big {
            lines.push(format!("{path} ({}, over GitHub's 100 MB limit)", size_text(*size)));
        }
        for (path, size) in &self.large {
            lines.push(format!("{path} ({}, large file)", size_text(*size)));
        }
        for path in &self.secret_files {
            lines.push(format!("{path} (looks like a secrets or key file)"));
        }
        for (path, what) in &self.secret_hits {
            lines.push(format!("{path} (contains what looks like a {what})"));
        }
        for (dir, files, size) in &self.heavy_dirs {
            lines.push(format!("{dir}/ ({files} files, {}, generated/dependency folder)", size_text(*size)));
        }
        lines
    }
}

impl ScanReport {
    /// Flagged paths that are safe to keep out with .gitignore, as
    /// `(path, is_folder)`. Source files with tokens in them aren't: they
    /// need the token taken out instead.
    pub fn ignorable(&self) -> Vec<(String, bool)> {
        let files = self.too_big.iter().chain(&self.large).map(|(path, _)| path);
        files
            .chain(&self.secret_files)
            .map(|path| (path.clone(), false))
            .chain(self.heavy_dirs.iter().map(|(dir, _, _)| (dir.clone(), true)))
            .collect()
    }
}

/// A .gitignore rule that keeps out `path`, a file or (when `folder`) a folder.
pub fn ignore_rule(path: &str, folder: bool) -> String {
    let name = path.rsplit('/').next().unwrap_or(path);
    if folder && HEAVY_DIRS.contains(&name) {
        // Generated folders are unwanted wherever they turn up.
        return format!("{}/", escape_rule(name));
    }
    let rule = format!("/{}", escape_rule(path));
    if folder { rule + "/" } else { rule }
}

/// Makes glob characters in a path match literally.
fn escape_rule(path: &str) -> String {
    let mut rule = String::new();
    for c in path.chars() {
        if matches!(c, '*' | '?' | '[' | '\\' | '!' | '#') {
            rule.push('\\');
        }
        rule.push(c);
    }
    rule
}

pub fn size_text(bytes: u64) -> String {
    match bytes {
        b if b >= 1024 * MB => format!("{:.1} GB", b as f64 / (1024 * MB) as f64),
        b if b >= MB => format!("{:.1} MB", b as f64 / MB as f64),
        b if b >= 1024 => format!("{:.0} KB", b as f64 / 1024.0),
        b => format!("{b} B"),
    }
}

/// Lists the files an upload would include and checks them.
pub fn scan(info: &ProjectInfo, job: Option<&Job>) -> ScanReport {
    let root = &info.path;
    let mut files: Vec<(String, u64)> = Vec::new();
    let mut report = ScanReport {
        via_git: info.is_git,
        ..Default::default()
    };
    if info.is_git {
        // Tracked files plus untracked ones that aren't ignored: exactly what `git add -A` would commit.
        let listing = git(root)
            .args(["ls-files", "-z", "--cached", "--others", "--exclude-standard"])
            .run(job);
        match listing {
            Ok(out) if out.ok => {
                for rel in out.stdout.split('\0').map(|p| p.trim_end_matches('\n')) {
                    if rel.is_empty() {
                        continue;
                    }
                    if let Ok(meta) = std::fs::metadata(root.join(rel)) {
                        if meta.is_file() {
                            files.push((rel.replace('\\', "/"), meta.len()));
                        }
                    }
                }
            }
            Ok(out) => report.error = Some(out.err_text()),
            Err(err) => report.error = Some(err),
        }
    } else {
        // Not a repo yet, so apply .gitignore rules the way git will after `git init`.
        let walker = ignore::WalkBuilder::new(root)
            .hidden(false)
            .ignore(false)
            .parents(false)
            .git_ignore(true)
            .git_exclude(true)
            .git_global(true)
            .require_git(false)
            .filter_entry(|entry| entry.file_name() != ".git")
            .build();
        for entry in walker.flatten() {
            if job.is_some_and(|job| job.cancel.is_cancelled()) {
                report.error = Some("Cancelled.".into());
                return report;
            }
            if entry.file_type().is_some_and(|t| t.is_file()) {
                let size = entry.metadata().map(|m| m.len()).unwrap_or(0);
                let rel = entry.path().strip_prefix(root).unwrap_or(entry.path());
                files.push((rel.to_string_lossy().replace('\\', "/"), size));
            }
        }
    }
    files.sort();
    check(root, &files, &mut report);
    report
}

fn check(root: &Path, files: &[(String, u64)], report: &mut ScanReport) {
    let mut heavy: BTreeMap<String, (usize, u64)> = BTreeMap::new();
    let mut top: BTreeMap<String, (usize, u64)> = BTreeMap::new();
    let mut read_budget: u64 = 300 * MB;
    for (rel, size) in files {
        report.files += 1;
        report.bytes += size;
        let parts: Vec<&str> = rel.split('/').collect();
        let name = parts.last().copied().unwrap_or_default();

        let first = if parts.len() > 1 { format!("{}/", parts[0]) } else { parts[0].to_string() };
        let entry = top.entry(first).or_default();
        entry.0 += 1;
        entry.1 += size;

        if *size > HARD_LIMIT {
            report.too_big.push((rel.clone(), *size));
        } else if *size > SOFT_LIMIT {
            report.large.push((rel.clone(), *size));
        }
        if is_secret_file(name) {
            report.secret_files.push(rel.clone());
        }
        if let Some(i) = parts[..parts.len() - 1].iter().position(|dir| HEAVY_DIRS.contains(dir)) {
            let entry = heavy.entry(parts[..=i].join("/")).or_default();
            entry.0 += 1;
            entry.1 += size;
        }
        if *size <= MB && *size <= read_budget && worth_reading(name) {
            read_budget -= size;
            if let Ok(bytes) = std::fs::read(root.join(rel)) {
                if !bytes[..bytes.len().min(8000)].contains(&0) {
                    if let Some(what) = find_secret(&String::from_utf8_lossy(&bytes)) {
                        report.secret_hits.push((rel.clone(), what));
                    }
                }
            }
        }
    }
    report.heavy_dirs = heavy.into_iter().map(|(dir, (n, b))| (dir, n, b)).collect();
    report.heavy_dirs.sort_by(|a, b| b.2.cmp(&a.2));
    report.top_level = top.into_iter().map(|(dir, (n, b))| (dir, n, b)).collect();
    report.top_level.sort_by(|a, b| b.2.cmp(&a.2));
}

fn is_secret_file(name: &str) -> bool {
    let name = name.to_lowercase();
    const TEMPLATES: &[&str] = &[".example", ".sample", ".template", ".dist", ".defaults"];
    if (name == ".env" || name.starts_with(".env.") || name.ends_with(".env"))
        && !TEMPLATES.iter().any(|t| name.ends_with(t))
    {
        return true;
    }
    let extension = name.rsplit_once('.').map(|(_, ext)| ext).unwrap_or_default();
    matches!(extension, "pem" | "key" | "p12" | "pfx" | "jks" | "keystore" | "kdbx" | "ppk")
        || matches!(
            name.as_str(),
            "id_rsa"
                | "id_dsa"
                | "id_ecdsa"
                | "id_ed25519"
                | "credentials.json"
                | ".pypirc"
                | ".netrc"
                | ".htpasswd"
                | "secrets.json"
                | "secrets.toml"
                | "secrets.yaml"
                | "secrets.yml"
        )
        || name.starts_with("client_secret")
        || (name.starts_with("service-account") && name.ends_with(".json"))
}

fn worth_reading(name: &str) -> bool {
    let name = name.to_lowercase();
    const SKIP: &[&str] = &[
        "png", "jpg", "jpeg", "gif", "ico", "webp", "bmp", "svg", "zip", "gz", "7z", "exe", "dll", "so",
        "pdb", "pdf", "mp3", "mp4", "wav", "ogg", "ttf", "otf", "woff", "woff2", "lock", "map",
    ];
    let extension = name.rsplit_once('.').map(|(_, ext)| ext).unwrap_or_default();
    !SKIP.contains(&extension) && !matches!(name.as_str(), "package-lock.json" | "pnpm-lock.yaml")
}

/// A token prefix plus what has to follow it, so mentions of the prefix in
/// code or docs (like this file) don't count.
struct TokenPattern {
    prefix: &'static str,
    min_len: usize,
    what: &'static str,
}

const TOKENS: &[TokenPattern] = &[
    TokenPattern { prefix: "sk-ant-", min_len: 40, what: "Anthropic API key" },
    TokenPattern { prefix: "ghp_", min_len: 36, what: "GitHub token" },
    TokenPattern { prefix: "gho_", min_len: 36, what: "GitHub token" },
    TokenPattern { prefix: "github_pat_", min_len: 60, what: "GitHub token" },
    TokenPattern { prefix: "AKIA", min_len: 16, what: "AWS access key" },
    TokenPattern { prefix: "AIza", min_len: 35, what: "Google API key" },
    TokenPattern { prefix: "sk-proj-", min_len: 40, what: "OpenAI API key" },
    TokenPattern { prefix: "sk_live_", min_len: 20, what: "Stripe secret key" },
    TokenPattern { prefix: "xoxb-", min_len: 20, what: "Slack token" },
    TokenPattern { prefix: "xoxp-", min_len: 20, what: "Slack token" },
    TokenPattern { prefix: "_authToken=", min_len: 20, what: "npm token" },
];

fn find_secret(text: &str) -> Option<&'static str> {
    let token_char = |c: &char| c.is_ascii_alphanumeric() || matches!(c, '_' | '-');
    for pattern in TOKENS {
        for (at, _) in text.match_indices(pattern.prefix) {
            let tail = text[at + pattern.prefix.len()..].chars().take_while(token_char).count();
            if tail >= pattern.min_len {
                return Some(pattern.what);
            }
        }
    }
    for (at, _) in text.match_indices("-----BEGIN ") {
        let header: String = text[at..].chars().take(50).collect();
        if header.contains(&["PRIVATE", " KEY-----"].concat()) {
            return Some("private key");
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn github_remotes() {
        let expected = Some(("VinZie".to_string(), "deadlock-toolbox".to_string()));
        assert_eq!(parse_github("https://github.com/VinZie/deadlock-toolbox.git"), expected);
        assert_eq!(parse_github("git@github.com:VinZie/deadlock-toolbox.git"), expected);
        assert_eq!(parse_github("https://github.com/VinZie/deadlock-toolbox/"), expected);
        assert_eq!(parse_github("https://gitlab.com/VinZie/deadlock-toolbox.git"), None);
    }

    #[test]
    fn repo_names() {
        assert_eq!(repo_name("skateboard globe"), "skateboard-globe");
        assert_eq!(repo_name("TBAnticheat"), "TBAnticheat");
        assert_eq!(repo_name("  weird!!name  "), "weird-name");
        assert_eq!(repo_name("!!!"), "project");
    }

    #[test]
    fn topic_cleanup() {
        assert_eq!(topics("Rust, egui  GUI,rust"), vec!["rust", "egui", "gui"]);
        assert_eq!(topics("C++ tools"), vec!["c", "tools"]);
    }

    #[test]
    fn ignore_rules() {
        assert_eq!(ignore_rule("node_modules", true), "node_modules/");
        assert_eq!(ignore_rule("src/__pycache__", true), "__pycache__/");
        assert_eq!(ignore_rule("assets/raw", true), "/assets/raw/");
        assert_eq!(ignore_rule(".env.local", false), "/.env.local");
        assert_eq!(ignore_rule("data/big file[1].bin", false), "/data/big file\\[1].bin");
    }

    #[test]
    fn secret_files() {
        assert!(is_secret_file(".env"));
        assert!(is_secret_file(".env.local"));
        assert!(!is_secret_file(".env.example"));
        assert!(is_secret_file("server.pem"));
        assert!(is_secret_file("id_ed25519"));
        assert!(!is_secret_file("main.rs"));
    }

    #[test]
    fn secret_contents() {
        let fake = format!("key = \"{}{}\"", "sk-ant-", "a".repeat(60));
        assert_eq!(find_secret(&fake), Some("Anthropic API key"));
        // Bare prefixes, as in code that looks for them, are fine.
        assert_eq!(find_secret("starts_with(\"sk-ant-\")"), None);
        let pem = ["-----BEGIN ", "RSA PRIVATE", " KEY-----\nabc"].concat();
        assert_eq!(find_secret(&pem), Some("private key"));
    }
}
