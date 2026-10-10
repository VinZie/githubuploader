//! The things the buttons do. Each runs on a job thread.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::claude::{self, Ask};
use crate::cmd::{Cmd, gh, git};
use crate::form::RepoForm;
use crate::github::{self, Account, CollabKind};
use crate::jobs::{Job, LogKind, Msg, Outcome, OutcomeKind, StepState, Tools};
use crate::project::{self, ProjectInfo, ScanReport, size_text};

pub fn tools() -> Tools {
    let version = |cmd: Cmd| {
        cmd.arg("--version")
            .run(None)
            .ok()
            .filter(|out| out.ok)
            .map(|out| out.text().lines().next().unwrap_or_default().to_string())
    };
    Tools {
        git: version(Cmd::new("git")),
        gh: version(gh()),
        claude: version(Cmd::new(claude::exe())),
    }
}

/// The global git identity, if both name and email are set.
pub fn identity() -> Option<(String, String)> {
    let get = |key: &str| {
        Cmd::new("git")
            .args(["config", "--global", key])
            .run(None)
            .ok()
            .filter(|out| out.ok)
            .map(|out| out.text().to_string())
            .filter(|value| !value.is_empty())
    };
    Some((get("user.name")?, get("user.email")?))
}

pub fn save_identity(job: &Job, name: &str, email: &str) -> Result<(), String> {
    Cmd::new("git").args(["config", "--global", "user.name", name]).echo().run_ok(Some(job))?;
    Cmd::new("git").args(["config", "--global", "user.email", email]).echo().run_ok(Some(job))?;
    job.ok(format!("git will now commit as {name} <{email}>"));
    job.send(Msg::Identity(identity()));
    Ok(())
}

pub fn load_projects(job: &Job, root: &Path) -> Result<(), String> {
    let dirs = project::list(root)?;
    let infos = std::thread::scope(|scope| {
        let handles: Vec<_> = dirs.iter().map(|dir| scope.spawn(|| project::inspect(dir))).collect();
        handles.into_iter().filter_map(|h| h.join().ok()).collect::<Vec<_>>()
    });
    job.info(format!("Found {} project folders in {}", infos.len(), root.display()));
    job.send(Msg::Projects(infos));
    Ok(())
}

/// Re-reads a project's state after something changed it.
pub fn refresh(job: &Job, path: &Path) -> ProjectInfo {
    let info = project::inspect(path);
    job.send(Msg::Project(info.clone()));
    info
}

pub fn load_remote(job: &Job, info: &ProjectInfo) {
    if let Some(slug) = info.slug() {
        job.send(Msg::Remote(info.path.clone(), github::load(job, &slug)));
    }
}

pub fn rescan(job: &Job, info: &ProjectInfo) -> ScanReport {
    let report = project::scan(info, Some(job));
    job.send(Msg::Scan(info.path.clone(), report.clone()));
    report
}

fn toast(job: &Job, kind: LogKind, text: impl Into<String>) {
    job.send(Msg::Toast(kind, text.into()));
}

fn first_line(text: &str) -> String {
    text.lines().next().unwrap_or_default().to_string()
}

/// Whether the scan lets an upload go ahead: `Err` when it can't,
/// `Ok(Some(reason))` when the user has to look at something first.
fn gate(report: &ScanReport, form: &RepoForm) -> Result<Option<String>, String> {
    if let Some(err) = &report.error {
        return Err(format!("Couldn't list the project's files: {err}"));
    }
    if !report.too_big.is_empty() {
        return Err(format!(
            "{} file(s) are over GitHub's 100 MB limit. Use Ignore in the pre-flight check, or let Claude fix it.",
            report.too_big.len()
        ));
    }
    if report.warnings() > 0 && !form.accept_warnings {
        return Ok(Some(format!(
            "the pre-flight check found {} thing(s) to review. Look them over, then tick \"upload anyway\" or ignore them.",
            report.warnings()
        )));
    }
    Ok(None)
}

fn scan_note(report: &ScanReport) -> String {
    format!("{} files, {}", report.files, size_text(report.bytes))
}

/// Stages everything and commits it. False when there was nothing to commit.
fn commit(job: &Job, dir: &Path, message: &str, account: &Account) -> Result<bool, String> {
    git(dir).args(["add", "-A"]).echo().run_ok(Some(job))?;
    let nothing_staged = git(dir).args(["diff", "--cached", "--quiet"]).run(Some(job))?.ok;
    if nothing_staged {
        return Ok(false);
    }
    let has_identity = git(dir)
        .args(["config", "user.email"])
        .run(Some(job))?
        .text()
        .contains('@');
    let mut cmd = git(dir);
    if !has_identity {
        let (name, email) = (account.display_name(), account.noreply_email());
        job.info(format!("git has no identity set, so committing as {name} <{email}>"));
        cmd = cmd.args(["-c", &format!("user.name={name}"), "-c", &format!("user.email={email}")]);
    }
    cmd.args(["commit", "-F", "-"])
        .stdin(message.trim())
        .echo()
        .stream()
        .run_ok(Some(job))?;
    Ok(true)
}

fn write_license(job: &Job, dir: &Path, key: &str, account: &Account) -> Result<(), String> {
    let text = github::license_text(job, key)?
        .replace("[year]", &current_year().to_string())
        .replace("[fullname]", account.display_name());
    std::fs::write(dir.join("LICENSE"), text).map_err(|err| format!("couldn't write LICENSE: {err}"))?;
    job.ok("Added a LICENSE file.");
    Ok(())
}

fn current_year() -> i64 {
    let days = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() / 86_400) as i64;
    // Howard Hinnant's days-to-civil-date algorithm.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    yoe + era * 400 + i64::from(mp >= 10)
}

// ---------------------------------------------------------------- uploading

/// The steps of an upload, mirrored in the project's progress list.
struct Plan<'a> {
    job: &'a Job,
    path: PathBuf,
    labels: Vec<String>,
}

impl<'a> Plan<'a> {
    fn new(job: &'a Job, path: &Path) -> Self {
        Self {
            job,
            path: path.to_path_buf(),
            labels: Vec::new(),
        }
    }

    fn add(&mut self, label: &str) -> usize {
        self.labels.push(label.to_string());
        self.labels.len() - 1
    }

    fn announce(&self) {
        self.job.send(Msg::Plan(self.path.clone(), self.labels.clone()));
    }

    fn set(&self, index: usize, state: StepState, note: Option<String>) {
        self.job.send(Msg::Step {
            path: self.path.clone(),
            index,
            state,
            note,
        });
    }

    /// Runs one step, marking it done or failed.
    fn run<T>(&self, index: usize, work: impl FnOnce() -> Result<T, String>) -> Result<T, String> {
        self.set(index, StepState::Running, None);
        let result = work();
        match &result {
            Ok(_) => self.set(index, StepState::Done, None),
            Err(err) => self.set(index, StepState::Failed, Some(first_line(err))),
        }
        result
    }
}

pub struct PublishOptions {
    pub account: Account,
    pub model: String,
    /// Whether Claude Code is installed.
    pub claude: bool,
}

enum Finish {
    Uploaded(String),
    NeedsReview(String),
}

/// Uploads a project: creates the GitHub repository and pushes it, or
/// pushes new commits when it's already there. True when it went up.
pub fn publish(job: &Job, info: &ProjectInfo, form: &RepoForm, options: &PublishOptions) -> Result<bool, String> {
    let new = info.github.is_none();
    let result = publish_steps(job, info, form, options);
    refresh(job, &info.path);
    let (outcome, uploaded) = match &result {
        Ok(Finish::Uploaded(url)) => {
            let text = if new { format!("{} is on GitHub", info.name) } else { format!("Pushed {} to GitHub", info.name) };
            job.ok(format!("{text}: {url}"));
            toast(job, LogKind::Ok, &text);
            let outcome = Outcome {
                kind: OutcomeKind::Success,
                text,
                url: Some(url.clone()),
            };
            (outcome, true)
        }
        Ok(Finish::NeedsReview(why)) => {
            let text = format!("Stopped before uploading: {why}");
            job.warn(&text);
            toast(job, LogKind::Warn, format!("{} needs a look before it can go up", info.name));
            let outcome = Outcome {
                kind: OutcomeKind::Attention,
                text,
                url: None,
            };
            (outcome, false)
        }
        Err(err) => {
            let outcome = Outcome {
                kind: OutcomeKind::Failed,
                text: err.clone(),
                url: None,
            };
            (outcome, false)
        }
    };
    job.send(Msg::Outcome(info.path.clone(), outcome));
    result.map(|_| uploaded)
}

fn publish_steps(
    job: &Job,
    info: &ProjectInfo,
    form: &RepoForm,
    options: &PublishOptions,
) -> Result<Finish, String> {
    let dir = info.path.as_path();
    let new = info.github.is_none();
    if info.empty {
        return Err("This folder is empty.".into());
    }
    if new && form.owner.is_empty() {
        return Err("Pick an owner for the repository.".into());
    }
    if let (true, Some(origin)) = (new, &info.origin) {
        return Err(format!(
            "This repo already has an `origin` remote that isn't GitHub ({origin}). Remove or rename it first."
        ));
    }
    let slug = info.slug().unwrap_or_else(|| form.slug());
    let use_claude = new && form.claude_prepare && options.claude;
    let mut form = form.clone();

    let mut plan = Plan::new(job, dir);
    let check = plan.add("Check the files");
    let prepare_step = use_claude.then(|| plan.add("Claude prepares the repo"));
    let commit_step = plan.add("Commit");
    let push_step = plan.add(if new { "Create the GitHub repo and push" } else { "Push to GitHub" });
    let extras = new.then(|| plan.add("Topics, features and invitations"));
    plan.announce();

    plan.set(check, StepState::Running, None);
    let mut report = rescan(job, info);
    match (gate(&report, &form), use_claude) {
        // Claude gets the chance to sort problems out before they count.
        (Ok(_), true) | (Ok(None), false) => plan.set(check, StepState::Done, Some(scan_note(&report))),
        (Err(err), true) if report.error.is_none() => {
            plan.set(check, StepState::Attention, Some(first_line(&err)));
        }
        (Err(err), _) => {
            plan.set(check, StepState::Failed, Some(first_line(&err)));
            return Err(err);
        }
        (Ok(Some(why)), false) => {
            plan.set(check, StepState::Attention, Some(why.clone()));
            return Ok(Finish::NeedsReview(why));
        }
    }

    let mut info = info.clone();
    if let Some(step) = prepare_step {
        plan.set(step, StepState::Running, None);
        let suggestion = match claude_prepare(job, &info, &form, Some(&report), &options.model) {
            Ok(suggestion) => suggestion,
            Err(err) => {
                plan.set(step, StepState::Failed, Some(first_line(&err)));
                return Err(err);
            }
        };
        if form.description.trim().is_empty() {
            form.description = suggestion.description.unwrap_or_default();
        }
        if form.topics.trim().is_empty() {
            form.topics = suggestion.topics.join(", ");
        }
        info = refresh(job, dir);
        report = rescan(job, &info);
        match gate(&report, &form) {
            Ok(None) => plan.set(step, StepState::Done, Some(format!("now {}", scan_note(&report)))),
            Ok(Some(why)) => {
                plan.set(step, StepState::Attention, Some(why.clone()));
                return Ok(Finish::NeedsReview(format!("Claude tidied things up, but {why}")));
            }
            Err(err) => {
                plan.set(step, StepState::Failed, Some(first_line(&err)));
                return Err(err);
            }
        }
    }

    let account = &options.account;
    let committed = plan.run(commit_step, || {
        if let Some(key) = form.license.key().filter(|_| !info.has_license) {
            write_license(job, dir, key, account)?;
        }
        if !info.is_git {
            git(dir).args(["init", "-b", "main"]).echo().run_ok(Some(job))?;
        }
        let message = match form.commit_message.trim() {
            "" if new => "Initial commit".to_string(),
            "" => format!("Update {}", info.name),
            message => message.to_string(),
        };
        let committed = commit(job, dir, &message, account)?;
        if !committed && !info.has_commits {
            return Err("There's nothing to commit.".into());
        }
        Ok(committed)
    })?;
    if !committed {
        plan.set(commit_step, StepState::Done, Some("no new changes".into()));
    }

    plan.run(push_step, || if new { create_repo(job, dir, &slug, &form) } else { push(job, dir) })?;

    if let Some(step) = extras {
        plan.set(step, StepState::Running, None);
        let mut problems = Vec::new();
        let mut edit = gh().args([
            "repo".to_string(),
            "edit".to_string(),
            slug.clone(),
            format!("--enable-projects={}", form.projects),
            format!("--enable-discussions={}", form.discussions),
        ]);
        let topics = form.topic_list();
        if !topics.is_empty() {
            edit = edit.args(["--add-topic".to_string(), topics.join(",")]);
        }
        if let Err(err) = edit.echo().run_ok(Some(job)) {
            problems.push(err);
        }
        if let Err(err) = sync_collaborators(job, &slug, &form) {
            problems.push(err);
        }
        if problems.is_empty() {
            let invited = form.collaborators.iter().filter(|c| !c.remove).count();
            let note = (invited > 0).then(|| format!("{invited} invited"));
            plan.set(step, StepState::Done, note);
        } else {
            for problem in &problems {
                job.warn(problem);
            }
            plan.set(step, StepState::Attention, Some(first_line(&problems[0])));
        }
    }
    Ok(Finish::Uploaded(format!("https://github.com/{slug}")))
}

fn create_repo(job: &Job, dir: &Path, slug: &str, form: &RepoForm) -> Result<(), String> {
    if github::exists(job, slug)? {
        return Err(format!("github.com/{slug} already exists. Pick a different name."));
    }
    let mut create = gh().cwd(dir).args([
        "repo",
        "create",
        slug,
        if form.private { "--private" } else { "--public" },
        "--source",
        ".",
        "--remote",
        "origin",
        "--push",
    ]);
    if !form.description.trim().is_empty() {
        create = create.args(["--description", form.description.trim()]);
    }
    if !form.homepage.trim().is_empty() {
        create = create.args(["--homepage", form.homepage.trim()]);
    }
    if !form.issues {
        create = create.arg("--disable-issues");
    }
    if !form.wiki {
        create = create.arg("--disable-wiki");
    }
    create.echo().stream().run_ok(Some(job)).map(drop)
}

fn push(job: &Job, dir: &Path) -> Result<(), String> {
    git(dir)
        .args(["push", "-u", "origin", "HEAD"])
        .echo()
        .stream()
        .run_ok(Some(job))
        .map(drop)
        .map_err(|err| {
            format!("{err}\nIf GitHub has commits you don't have locally, pull them first (git pull) and try again.")
        })
}

pub fn publish_many(job: &Job, items: Vec<(ProjectInfo, RepoForm)>, options: &PublishOptions) -> Result<(), String> {
    let total = items.len();
    let (mut uploaded, mut failed) = (0, 0);
    for (info, form) in items {
        job.check()?;
        match publish(&job.tagged(&info.name), &info, &form, options) {
            Ok(true) => uploaded += 1,
            Ok(false) => {}
            Err(err) => {
                if job.cancel.is_cancelled() {
                    return Err(err);
                }
                failed += 1;
                job.tagged(&info.name).log(LogKind::Err, err);
            }
        }
    }
    let waiting = total - uploaded - failed;
    let mut summary = format!("Uploaded {uploaded} of {total} projects");
    if waiting > 0 {
        summary += &format!(", {waiting} need a look");
    }
    if failed > 0 {
        summary += &format!(", {failed} failed");
    }
    let kind = if uploaded == total { LogKind::Ok } else { LogKind::Warn };
    job.log(kind, &summary);
    toast(job, kind, summary);
    Ok(())
}

/// Sends changed settings and collaborators to an existing GitHub repository.
pub fn apply(job: &Job, info: &ProjectInfo, form: &RepoForm) -> Result<(), String> {
    let slug = info.slug().ok_or("This project isn't on GitHub yet.")?;
    let remote = form.remote.as_ref().ok_or("GitHub settings haven't loaded yet.")?;
    let mut args: Vec<String> = Vec::new();
    if form.description != remote.description {
        args.extend(["--description".into(), form.description.clone()]);
    }
    if form.homepage != remote.homepage {
        args.extend(["--homepage".into(), form.homepage.clone()]);
    }
    if form.private != remote.private {
        let visibility = if form.private { "private" } else { "public" };
        args.extend([
            "--visibility".into(),
            visibility.into(),
            "--accept-visibility-change-consequences".into(),
        ]);
    }
    for (now, was, feature) in [
        (form.issues, remote.issues, "issues"),
        (form.wiki, remote.wiki, "wiki"),
        (form.projects, remote.projects, "projects"),
        (form.discussions, remote.discussions, "discussions"),
    ] {
        if now != was {
            args.push(format!("--enable-{feature}={now}"));
        }
    }
    let topics = form.topic_list();
    let added: Vec<&str> = topics.iter().filter(|t| !remote.topics.contains(t)).map(String::as_str).collect();
    let removed: Vec<&str> = remote.topics.iter().filter(|t| !topics.contains(t)).map(String::as_str).collect();
    if !added.is_empty() {
        args.extend(["--add-topic".into(), added.join(",")]);
    }
    if !removed.is_empty() {
        args.extend(["--remove-topic".into(), removed.join(",")]);
    }

    let mut changed = false;
    if !args.is_empty() {
        gh().args(["repo", "edit", &slug]).args(args).echo().run_ok(Some(job))?;
        changed = true;
    }
    changed |= sync_collaborators(job, &slug, form)?;
    if changed {
        job.ok("Settings applied.");
        toast(job, LogKind::Ok, format!("Saved {}'s settings to GitHub", info.name));
    } else {
        job.info("Nothing to change.");
    }
    load_remote(job, info);
    Ok(())
}

/// Brings GitHub's collaborators in line with the form. True if anything changed.
fn sync_collaborators(job: &Job, slug: &str, form: &RepoForm) -> Result<bool, String> {
    let mut changed = false;
    for c in &form.collaborators {
        let edited = c.perm != c.saved_perm;
        match (c.kind, c.remove) {
            (CollabKind::New, false) => {
                github::put_collaborator(job, slug, &c.login, c.perm)?;
                job.ok(format!("Invited {} ({})", c.login, c.perm.label()));
            }
            (CollabKind::Member, true) => {
                github::remove_collaborator(job, slug, &c.login)?;
                job.ok(format!("Removed {}", c.login));
            }
            (CollabKind::Member, false) if edited => {
                github::put_collaborator(job, slug, &c.login, c.perm)?;
                job.ok(format!("{} now has {} access", c.login, c.perm.label()));
            }
            (CollabKind::Invited(id), true) => {
                github::cancel_invitation(job, slug, id)?;
                job.ok(format!("Withdrew the invitation to {}", c.login));
            }
            (CollabKind::Invited(id), false) if edited => {
                github::update_invitation(job, slug, id, c.perm)?;
                job.ok(format!("{}'s invitation is now for {} access", c.login, c.perm.label()));
            }
            _ => continue,
        }
        changed = true;
    }
    Ok(changed)
}

/// Adds .gitignore rules for flagged paths and stops git tracking them.
pub fn ignore(job: &Job, info: &ProjectInfo, paths: &[(String, bool)]) -> Result<(), String> {
    let file = info.path.join(".gitignore");
    let existing = std::fs::read_to_string(&file).unwrap_or_default();
    let present: HashSet<&str> = existing.lines().map(str::trim).collect();
    let mut rules: Vec<String> = Vec::new();
    for (path, folder) in paths {
        let rule = project::ignore_rule(path, *folder);
        if !present.contains(rule.as_str()) && !rules.contains(&rule) {
            rules.push(rule);
        }
    }
    if !rules.is_empty() {
        let mut text = existing.clone();
        if !text.is_empty() {
            text.push_str(if text.ends_with('\n') { "\n" } else { "\n\n" });
        }
        text.push_str("# Kept out of GitHub by GitHub Uploader\n");
        for rule in &rules {
            text.push_str(rule);
            text.push('\n');
        }
        std::fs::write(&file, text).map_err(|err| format!("couldn't update .gitignore: {err}"))?;
        job.ok(format!("Added to .gitignore: {}", rules.join("  ")));
    }
    if info.is_git {
        // Ignoring doesn't drop files git already tracks; this does, leaving them on disk.
        for (path, _) in paths {
            git(&info.path)
                .args(["rm", "-r", "--cached", "--ignore-unmatch", "--quiet", "--", path])
                .echo()
                .run_ok(Some(job))?;
        }
    }
    let info = refresh(job, &info.path);
    let report = rescan(job, &info);
    let left = report.warnings() + report.too_big.len();
    toast(job, LogKind::Ok, format!("Ignored {} path(s) in {}; {left} flag(s) left", paths.len(), info.name));
    Ok(())
}

// ---------------------------------------------------------------- Claude

const DESCRIBE_JSON: &str = r#"{"description": "<one sentence under 120 characters, no emoji>", "topics": ["<up to 6 lowercase GitHub topics>"]}"#;

#[derive(Default)]
struct Suggestion {
    description: Option<String>,
    topics: Vec<String>,
}

/// Claude makes .gitignore keep junk and secrets out, writes a missing
/// README, and suggests a description and topics. The description and
/// topics only fill fields the user left empty.
fn claude_prepare(
    job: &Job,
    info: &ProjectInfo,
    form: &RepoForm,
    report: Option<&ScanReport>,
    model: &str,
) -> Result<Suggestion, String> {
    let visibility = if form.private { "private" } else { "public" };
    let mut prompt = format!(
        "You're helping publish the project in the current directory to GitHub as a {visibility} repository named \"{}\".\n\n\
         Please get it ready:\n\
         1. Look around the project (README, manifest files such as Cargo.toml, package.json or pyproject.toml, and the source layout) to understand what it is.\n\
         2. Make sure .gitignore keeps dependency folders, build output, caches, virtual environments, large binaries and secrets (.env files, keys, credentials) out of the repository. Create .gitignore if it's missing; otherwise only add rules, never remove existing ones.\n",
        project::repo_name(&form.name)
    );
    let problems = report.map(ScanReport::problems).unwrap_or_default();
    if !problems.is_empty() {
        prompt += "   A pre-flight scan found these paths that would currently be uploaded:\n";
        for problem in problems.iter().take(60) {
            prompt += &format!("   - {problem}\n");
        }
        prompt += "   Add .gitignore rules for the ones that shouldn't be published. If a source file contains a \
                   key or token, move the value into an environment variable instead.\n";
        if info.is_git {
            prompt += "   Files git already tracks stay in the repository even once ignored; untrack those with \
                       `git rm -r --cached <path>`, which keeps them on disk.\n";
        }
    }
    prompt += if info.has_readme {
        "3. A README already exists. Leave it unchanged.\n"
    } else {
        "3. There's no README. Write a concise README.md: a one-paragraph summary, the main features, and how to build and run it. Only describe what the code actually does.\n"
    };
    prompt += &format!(
        "\nDon't change any other files.\n\n\
         End your reply with one line holding only a JSON object: {DESCRIBE_JSON}"
    );
    let mut tools = claude::EDIT.to_vec();
    if info.is_git && !problems.is_empty() {
        tools.extend(["Bash(git rm:*)", "Bash(git ls-files:*)", "Bash(git status:*)"]);
    }
    let answer = claude::ask(
        job,
        Ask {
            dir: &info.path,
            prompt,
            model,
            tools,
        },
    )?;
    Ok(send_suggestion(job, &info.path, &answer, false))
}

/// The "Prepare for GitHub" button: Claude's preparation on its own.
pub fn prepare(job: &Job, info: &ProjectInfo, form: &RepoForm, report: Option<&ScanReport>, model: &str) -> Result<(), String> {
    claude_prepare(job, info, form, report, model)?;
    after_claude(job, &info.path);
    toast(job, LogKind::Ok, format!("Claude prepared {}", info.name));
    Ok(())
}

/// Claude suggests a description and topics without changing anything.
pub fn suggest(job: &Job, info: &ProjectInfo, model: &str) -> Result<(), String> {
    let prompt = format!(
        "Look at the project in the current directory (README, manifest files and source layout) and describe it \
         for its GitHub page. Don't change anything. Reply with only a JSON object: {DESCRIBE_JSON}"
    );
    let answer = claude::ask(
        job,
        Ask {
            dir: &info.path,
            prompt,
            model,
            tools: claude::READ_ONLY.to_vec(),
        },
    )?;
    send_suggestion(job, &info.path, &answer, true);
    Ok(())
}

fn send_suggestion(job: &Job, path: &Path, answer: &str, overwrite: bool) -> Suggestion {
    let Some(json) = claude::last_json(answer) else {
        job.warn("Claude didn't send back a description, so the form is unchanged.");
        return Suggestion::default();
    };
    let description = json["description"]
        .as_str()
        .map(|d| d.trim().to_string())
        .filter(|d| !d.is_empty());
    let topics: Vec<String> = json["topics"]
        .as_array()
        .map(|topics| topics.iter().filter_map(|t| t.as_str().map(str::to_string)).collect())
        .unwrap_or_default();
    job.send(Msg::Suggestion {
        path: path.to_path_buf(),
        description: description.clone(),
        topics: topics.clone(),
        overwrite,
    });
    job.ok("Got a description and topics from Claude.");
    Suggestion { description, topics }
}

/// Claude may have changed files, so re-read the project and rescan it.
fn after_claude(job: &Job, path: &Path) {
    let info = refresh(job, path);
    rescan(job, &info);
}

pub fn commit_message(job: &Job, info: &ProjectInfo, model: &str) -> Result<(), String> {
    let dir = &info.path;
    let context = if info.has_commits {
        let status = git(dir).args(["status", "--porcelain"]).run_ok(Some(job))?;
        if status.trim().is_empty() {
            return Err("There are no changes to describe.".into());
        }
        let stat = git(dir).args(["diff", "HEAD", "--stat"]).run_ok(Some(job)).unwrap_or_default();
        let diff = git(dir).args(["diff", "HEAD"]).run_ok(Some(job)).unwrap_or_default();
        format!(
            "git status:\n{}\n\ngit diff --stat:\n{}\n\ngit diff (may be cut short):\n{}",
            cut(&status, 6_000),
            cut(&stat, 4_000),
            cut(&diff, 20_000)
        )
    } else {
        "This is the project's first commit. Look around the project to see what it contains.".into()
    };
    let prompt = format!(
        "Write a git commit message for the project in the current directory. Use the imperative mood and a \
         subject line under 72 characters, optionally followed by a blank line and a few short bullet points. \
         Reply with only the commit message: no code fences, no preamble.\n\n{context}"
    );
    let answer = claude::ask(
        job,
        Ask {
            dir,
            prompt,
            model,
            tools: claude::READ_ONLY.to_vec(),
        },
    )?;
    let message = claude::unfence(&answer);
    if message.is_empty() {
        return Err("Claude returned an empty message.".into());
    }
    job.send(Msg::CommitMsg(dir.clone(), message));
    Ok(())
}

fn cut(text: &str, max: usize) -> String {
    if text.len() <= max {
        return text.to_string();
    }
    let mut end = max;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}\n…(cut short)", &text[..end])
}

/// Claude adds .gitignore rules (and untracks files) for what the pre-flight check flagged.
pub fn fix_problems(job: &Job, info: &ProjectInfo, report: &ScanReport, model: &str) -> Result<(), String> {
    let problems = report.problems();
    if problems.is_empty() {
        return Err("The pre-flight check found nothing to fix.".into());
    }
    let mut prompt = String::from(
        "These paths in the project in the current directory would be uploaded to GitHub, but look like they shouldn't be:\n",
    );
    for problem in problems.iter().take(80) {
        prompt += &format!("- {problem}\n");
    }
    prompt += "\nUpdate .gitignore (create it if it's missing; only add rules, never remove any) so generated \
               folders, oversized files and secrets stay out of the repository. If a source file contains a key or \
               token, move the value into an environment variable instead. If a flagged file is genuinely source \
               that belongs in the repository, leave it and say why.";
    let mut tools = claude::EDIT.to_vec();
    if info.is_git {
        prompt += " This is already a git repository, and files git already tracks stay in it even once \
                   they're ignored: untrack those with `git rm -r --cached <path>`, which keeps them on disk.";
        tools.extend(["Bash(git rm:*)", "Bash(git ls-files:*)", "Bash(git status:*)", "Bash(git check-ignore:*)"]);
    }
    prompt += " Finish with a short summary of what you changed.";
    claude::ask(
        job,
        Ask {
            dir: &info.path,
            prompt,
            model,
            tools,
        },
    )?;
    after_claude(job, &info.path);
    toast(job, LogKind::Ok, format!("Claude worked through {}'s flags", info.name));
    Ok(())
}

/// Runs the chosen Claude tasks on one project.
pub fn run_tasks(job: &Job, info: &ProjectInfo, request: &str, model: &str, may_run_git: bool) -> Result<(), String> {
    let mut prompt = format!(
        "You're working on the project in the current directory, which the user is preparing to publish on \
         GitHub with a desktop uploader app.\n\n{request}"
    );
    let mut tools = claude::EDIT.to_vec();
    if may_run_git {
        tools.extend(claude::GIT);
        prompt += "\n\nYou may run git and gh commands with the Bash tool.";
    } else {
        prompt += "\n\nYou can read and edit files but can't run commands; the app takes care of git and GitHub.";
    }
    claude::ask(
        job,
        Ask {
            dir: &info.path,
            prompt,
            model,
            tools,
        },
    )?;
    after_claude(job, &info.path);
    Ok(())
}

/// Runs the same Claude tasks on several projects, one after another.
pub fn run_tasks_many(
    job: &Job,
    infos: Vec<ProjectInfo>,
    request: &str,
    model: &str,
    may_run_git: bool,
) -> Result<(), String> {
    let total = infos.len();
    let mut failed = Vec::new();
    for info in infos {
        job.check()?;
        let job = job.tagged(&info.name);
        if let Err(err) = run_tasks(&job, &info, request, model, may_run_git) {
            if job.cancel.is_cancelled() {
                return Err(err);
            }
            job.log(LogKind::Err, err);
            failed.push(info.name);
        }
    }
    if failed.is_empty() {
        let text = if total == 1 { "Claude finished".to_string() } else { format!("Claude finished all {total} projects") };
        toast(job, LogKind::Ok, text);
        Ok(())
    } else {
        Err(format!("Claude couldn't finish {}", failed.join(", ")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn year_is_plausible() {
        let year = current_year();
        assert!((2024..2100).contains(&year), "{year}");
    }

    #[test]
    fn cut_respects_char_boundaries() {
        assert_eq!(cut("abc", 10), "abc");
        assert!(cut("ééééé", 3).starts_with('é'));
    }
}
