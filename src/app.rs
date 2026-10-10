//! The window: projects on the left, the selected project in the middle,
//! and the activity log along the bottom.

use std::collections::{HashMap, HashSet};
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::mpsc::{self, Receiver};
use std::time::{Duration, Instant};

use eframe::egui::{self, Color32, RichText, Ui, vec2};
use egui::text::{LayoutJob, TextFormat};

use crate::cmd::NO_WINDOW;
use crate::config::{Config, SavedPrompt};
use crate::form::{License, RepoForm};
use crate::github::{self, Account, Collab, CollabKind, Perm};
use crate::jobs::{Bus, Cancel, Job, LogKind, LogLine, Msg, Outcome, OutcomeKind, StepState, Tools};
use crate::ops::{self, PublishOptions};
use crate::project::{self, ProjectInfo, ScanReport, size_text};
use crate::tasks;

const GREEN: Color32 = Color32::from_rgb(0x73, 0xc9, 0x91);
const YELLOW: Color32 = Color32::from_rgb(0xe0, 0xaf, 0x68);
const RED: Color32 = Color32::from_rgb(0xf7, 0x76, 0x8e);
const BLUE: Color32 = Color32::from_rgb(0x7a, 0xa2, 0xf7);
const TEAL: Color32 = Color32::from_rgb(0x7d, 0xcf, 0xff);
const CLAUDE: Color32 = Color32::from_rgb(0xe8, 0xa0, 0x7a);
const ACCENT: Color32 = Color32::from_rgb(0x2f, 0x81, 0x4f);

const LOG_LIMIT: usize = 2_000;
const TOAST_TIME: Duration = Duration::from_secs(6);

const MODELS: [(&str, &str); 4] = [
    ("", "Claude Code default"),
    ("haiku", "Haiku (fastest)"),
    ("sonnet", "Sonnet"),
    ("opus", "Opus (most capable)"),
];

#[derive(Clone)]
enum Action {
    Select(PathBuf),
    ReloadProjects,
    BrowseRoot,
    SignIn,
    RefreshAccount,
    SaveIdentity,
    OpenSettings,
    SaveSettings,
    LoadRemote(PathBuf),
    Rescan(PathBuf),
    Refresh(PathBuf),
    Publish { path: PathBuf, confirmed: bool },
    ReviewBatch,
    PublishChecked,
    Apply { path: PathBuf, confirmed: bool },
    Prepare(PathBuf),
    Suggest(PathBuf),
    CommitMessage(PathBuf),
    FixProblems(PathBuf),
    Ignore { path: PathBuf, paths: Vec<(String, bool)> },
    RunTasks { path: PathBuf, on_checked: bool },
    SavePrompt(PathBuf),
    DeletePrompt(String),
    CopyToChecked(PathBuf),
    DismissOutcome(PathBuf),
    Open(String),
    CopyText(String),
    Cancel(u64),
}

struct Running {
    id: u64,
    label: String,
    paths: Vec<PathBuf>,
    cancel: Cancel,
    started: Instant,
    /// Uploads and Claude tasks lock their projects; scans and lookups don't.
    blocking: bool,
}

struct Confirm {
    title: String,
    body: String,
    yes: String,
    action: Action,
}

struct Step {
    label: String,
    state: StepState,
    note: Option<String>,
}

/// The last upload of a project: its steps and how it ended.
#[derive(Default)]
struct Progress {
    steps: Vec<Step>,
    outcome: Option<Outcome>,
}

struct Toast {
    kind: LogKind,
    text: String,
    until: Instant,
}

/// The Settings window's fields while it's open.
struct SettingsDraft {
    model: String,
    private: bool,
    license: License,
    collaborators: String,
    claude_prepares: bool,
}

pub struct App {
    cfg: Config,
    bus: Bus,
    rx: Receiver<Msg>,
    tools: Option<Tools>,
    account: Option<Result<Account, String>>,
    /// Outer `None` while still checking.
    identity: Option<Option<(String, String)>>,
    projects: Vec<ProjectInfo>,
    forms: HashMap<PathBuf, RepoForm>,
    scans: HashMap<PathBuf, ScanReport>,
    scanning: HashSet<PathBuf>,
    remote_loading: HashSet<PathBuf>,
    progress: HashMap<PathBuf, Progress>,
    checked: HashSet<PathBuf>,
    selected: Option<PathBuf>,
    running: Vec<Running>,
    next_job: u64,
    log: Vec<LogLine>,
    toasts: Vec<Toast>,
    confirm: Option<Confirm>,
    settings: Option<SettingsDraft>,
    batch_review: bool,
}

impl App {
    pub fn new(cc: &eframe::CreationContext) -> Self {
        style(&cc.egui_ctx);
        let (tx, rx) = mpsc::channel();
        let mut app = Self {
            cfg: Config::load(),
            bus: Bus::new(tx, cc.egui_ctx.clone()),
            rx,
            tools: None,
            account: None,
            identity: None,
            projects: Vec::new(),
            forms: HashMap::new(),
            scans: HashMap::new(),
            scanning: HashSet::new(),
            remote_loading: HashSet::new(),
            progress: HashMap::new(),
            checked: HashSet::new(),
            selected: None,
            running: Vec::new(),
            next_job: 1,
            log: Vec::new(),
            toasts: Vec::new(),
            confirm: None,
            settings: None,
            batch_review: false,
        };
        app.background("Checking tools", vec![], "setup", |job| {
            job.send(Msg::Tools(ops::tools()));
            job.send(Msg::Identity(ops::identity()));
            Ok(())
        });
        app.handle(Action::RefreshAccount);
        app.handle(Action::ReloadProjects);
        app
    }

    /// Runs work that locks its projects until it finishes.
    fn spawn(
        &mut self,
        label: impl Into<String>,
        paths: Vec<PathBuf>,
        tag: &str,
        work: impl FnOnce(&Job) -> Result<(), String> + Send + 'static,
    ) {
        self.start(label.into(), paths, tag, true, work);
    }

    /// Runs a lookup that can happen alongside anything else.
    fn background(
        &mut self,
        label: impl Into<String>,
        paths: Vec<PathBuf>,
        tag: &str,
        work: impl FnOnce(&Job) -> Result<(), String> + Send + 'static,
    ) {
        self.start(label.into(), paths, tag, false, work);
    }

    fn start(
        &mut self,
        label: String,
        paths: Vec<PathBuf>,
        tag: &str,
        blocking: bool,
        work: impl FnOnce(&Job) -> Result<(), String> + Send + 'static,
    ) {
        let id = self.next_job;
        self.next_job += 1;
        let cancel = Cancel::default();
        let job = Job {
            id,
            tag: tag.to_string(),
            bus: self.bus.clone(),
            cancel: cancel.clone(),
        };
        self.running.push(Running {
            id,
            label: label.clone(),
            paths,
            cancel,
            started: Instant::now(),
            blocking,
        });
        std::thread::spawn(move || {
            if let Err(err) = work(&job) {
                if job.cancel.is_cancelled() {
                    job.warn("Cancelled.");
                    job.send(Msg::Toast(LogKind::Warn, format!("{label}: cancelled")));
                } else {
                    job.log(LogKind::Err, &err);
                    let first = err.lines().next().unwrap_or_default();
                    job.send(Msg::Toast(LogKind::Err, format!("{label} failed: {first}")));
                }
            }
            job.send(Msg::JobDone(job.id));
        });
    }

    fn busy(&self, path: &Path) -> bool {
        self.running.iter().any(|r| r.blocking && r.paths.iter().any(|p| p == path))
    }

    fn info(&self, path: &Path) -> Option<ProjectInfo> {
        self.projects.iter().find(|p| p.path == path).cloned()
    }

    fn account(&self) -> Option<&Account> {
        self.account.as_ref().and_then(|a| a.as_ref().ok())
    }

    fn claude_ready(&self) -> bool {
        self.tools.as_ref().is_some_and(|t| t.claude.is_some())
    }

    fn form_for(&mut self, info: &ProjectInfo) -> &mut RepoForm {
        let owner = self.account().map(|a| a.login.clone()).unwrap_or_default();
        let cfg = &self.cfg;
        self.forms
            .entry(info.path.clone())
            .or_insert_with(|| RepoForm::new(info, &owner, cfg))
    }

    /// Checked projects that can be uploaded, in list order.
    fn checked_projects(&self) -> Vec<ProjectInfo> {
        self.projects
            .iter()
            .filter(|p| self.checked.contains(&p.path) && !p.empty)
            .cloned()
            .collect()
    }

    fn publish_options(&self) -> Option<PublishOptions> {
        Some(PublishOptions {
            account: self.account()?.clone(),
            model: self.cfg.model.clone(),
            claude: self.claude_ready(),
        })
    }

    fn toast(&mut self, kind: LogKind, text: impl Into<String>) {
        self.toasts.push(Toast {
            kind,
            text: text.into(),
            until: Instant::now() + TOAST_TIME,
        });
    }

    fn drain(&mut self) {
        let mut follow_ups = Vec::new();
        while let Ok(msg) = self.rx.try_recv() {
            match msg {
                Msg::Log(line) => {
                    self.log.push(line);
                    if self.log.len() > LOG_LIMIT {
                        self.log.drain(..LOG_LIMIT / 4);
                    }
                }
                Msg::Tools(tools) => self.tools = Some(tools),
                Msg::Identity(identity) => self.identity = Some(identity),
                Msg::Account(account) => {
                    if let Ok(account) = &account {
                        for form in self.forms.values_mut().filter(|f| f.owner.is_empty()) {
                            form.owner = account.login.clone();
                        }
                    }
                    self.account = Some(account);
                }
                Msg::Projects(list) => {
                    for info in &list {
                        self.form_for(info);
                        if info.github.is_some() {
                            follow_ups.push(Action::LoadRemote(info.path.clone()));
                        }
                    }
                    self.checked.retain(|p| list.iter().any(|i| &i.path == p));
                    if self.selected.as_ref().is_some_and(|s| !list.iter().any(|i| &i.path == s)) {
                        self.selected = None;
                    }
                    self.projects = list;
                    if let Some(selected) = &self.selected {
                        follow_ups.push(Action::Rescan(selected.clone()));
                    }
                }
                Msg::Project(info) => {
                    let form = self.form_for(&info);
                    // Uploaded just now, or Claude pointed it at GitHub: follow the real remote.
                    if let Some((owner, name)) = &info.github {
                        form.owner = owner.clone();
                        form.name = name.clone();
                        if form.remote.is_none() {
                            follow_ups.push(Action::LoadRemote(info.path.clone()));
                        }
                    }
                    match self.projects.iter_mut().find(|p| p.path == info.path) {
                        Some(slot) => *slot = info,
                        None => self.projects.push(info),
                    }
                }
                Msg::Remote(path, result) => {
                    self.remote_loading.remove(&path);
                    if let Some(form) = self.forms.get_mut(&path) {
                        match result {
                            Ok(remote) => form.load_remote(remote),
                            Err(err) => form.remote_error = Some(err),
                        }
                    }
                }
                Msg::Scan(path, report) => {
                    self.scanning.remove(&path);
                    self.scans.insert(path, report);
                }
                Msg::Suggestion {
                    path,
                    description,
                    topics,
                    overwrite,
                } => {
                    if let Some(form) = self.forms.get_mut(&path) {
                        if let Some(description) = description {
                            if overwrite || form.description.trim().is_empty() {
                                form.description = description;
                            }
                        }
                        if !topics.is_empty() && (overwrite || form.topics.trim().is_empty()) {
                            form.topics = project::topics(&topics.join(",")).join(", ");
                        }
                    }
                }
                Msg::CommitMsg(path, message) => {
                    if let Some(form) = self.forms.get_mut(&path) {
                        form.commit_message = message;
                    }
                }
                Msg::Plan(path, labels) => {
                    let steps = labels
                        .into_iter()
                        .map(|label| Step {
                            label,
                            state: StepState::Pending,
                            note: None,
                        })
                        .collect();
                    self.progress.insert(path, Progress { steps, outcome: None });
                }
                Msg::Step {
                    path,
                    index,
                    state,
                    note,
                } => {
                    if let Some(step) = self.progress.get_mut(&path).and_then(|p| p.steps.get_mut(index)) {
                        step.state = state;
                        if note.is_some() {
                            step.note = note;
                        }
                    }
                }
                Msg::Outcome(path, outcome) => {
                    let progress = self.progress.entry(path).or_default();
                    if outcome.kind == OutcomeKind::Failed {
                        // A step cut short by an error or Cancel.
                        for step in progress.steps.iter_mut().filter(|s| s.state == StepState::Running) {
                            step.state = StepState::Failed;
                        }
                    }
                    progress.outcome = Some(outcome);
                }
                Msg::Toast(kind, text) => self.toast(kind, text),
                Msg::JobDone(id) => self.running.retain(|r| r.id != id),
            }
        }
        for action in follow_ups {
            self.handle(action);
        }
    }

    fn handle(&mut self, action: Action) {
        let model = self.cfg.model.clone();
        match action {
            Action::Select(path) => {
                self.selected = Some(path.clone());
                if !self.scans.contains_key(&path) {
                    self.handle(Action::Rescan(path));
                }
            }
            Action::ReloadProjects => {
                let root = PathBuf::from(self.cfg.root.trim());
                self.cfg.save();
                self.scans.clear();
                self.background("Finding projects", vec![], "projects", move |job| ops::load_projects(job, &root));
            }
            Action::BrowseRoot => {
                if let Some(dir) = rfd::FileDialog::new().set_directory(self.cfg.root.trim()).pick_folder() {
                    self.cfg.root = dir.display().to_string();
                    self.handle(Action::ReloadProjects);
                }
            }
            Action::SignIn => {
                // gh's browser sign-in needs a real terminal to show its one-time code.
                let _ = Command::new("cmd")
                    .raw_arg("/C start \"GitHub sign-in\" cmd /K gh auth login --web --git-protocol https")
                    .creation_flags(NO_WINDOW)
                    .spawn();
            }
            Action::RefreshAccount => {
                self.account = None;
                self.background("Checking GitHub account", vec![], "github", |job| {
                    job.send(Msg::Account(github::account(Some(job))));
                    Ok(())
                });
            }
            Action::SaveIdentity => {
                let Some(account) = self.account() else { return };
                let (name, email) = (account.display_name().to_string(), account.noreply_email());
                self.spawn("Saving git identity", vec![], "git", move |job| {
                    ops::save_identity(job, &name, &email)
                });
            }
            Action::OpenSettings => {
                self.settings = Some(SettingsDraft {
                    model: self.cfg.model.clone(),
                    private: self.cfg.default_private,
                    license: License::from_key(&self.cfg.default_license),
                    collaborators: self.cfg.default_collaborators.join(", "),
                    claude_prepares: self.cfg.claude_prepares,
                });
            }
            Action::SaveSettings => {
                let Some(draft) = self.settings.take() else { return };
                self.cfg.model = draft.model;
                self.cfg.default_private = draft.private;
                self.cfg.default_license = draft.license.key().unwrap_or_default().to_string();
                self.cfg.default_collaborators = draft
                    .collaborators
                    .split([',', ' ', '\n'])
                    .map(clean_login)
                    .filter(|login| valid_login(login))
                    .collect();
                self.cfg.claude_prepares = draft.claude_prepares;
                self.cfg.save();
                let mut updated = 0;
                for info in self.projects.iter().filter(|p| p.github.is_none() && !p.empty) {
                    if let Some(form) = self.forms.get_mut(&info.path) {
                        form.apply_defaults(&self.cfg);
                        updated += 1;
                    }
                }
                self.toast(LogKind::Ok, format!("Settings saved and applied to {updated} project(s) not on GitHub yet"));
            }
            Action::LoadRemote(path) => {
                let Some(info) = self.info(&path) else { return };
                if self.remote_loading.insert(path.clone()) {
                    self.background("Loading GitHub settings", vec![path], &info.name.clone(), move |job| {
                        ops::load_remote(job, &info);
                        Ok(())
                    });
                }
            }
            Action::Rescan(path) => {
                let Some(info) = self.info(&path) else { return };
                if self.scanning.insert(path.clone()) {
                    self.scans.remove(&path);
                    self.background("Checking files", vec![path], &info.name.clone(), move |job| {
                        ops::rescan(job, &info);
                        Ok(())
                    });
                }
            }
            Action::Refresh(path) => {
                if !self.scanning.insert(path.clone()) {
                    return;
                }
                self.scans.remove(&path);
                if let Some(form) = self.forms.get_mut(&path) {
                    form.remote = None;
                }
                let name = self.info(&path).map(|i| i.name).unwrap_or_default();
                self.background("Refreshing", vec![path.clone()], &name, move |job| {
                    let info = ops::refresh(job, &path);
                    ops::rescan(job, &info);
                    Ok(())
                });
            }
            Action::Publish { path, confirmed } => {
                let (Some(info), Some(options)) = (self.info(&path), self.publish_options()) else { return };
                let form = self.form_for(&info).clone();
                if !confirmed && !form.private && info.github.is_none() {
                    self.confirm = Some(Confirm {
                        title: "Publish publicly?".into(),
                        body: format!(
                            "github.com/{} will be created as a PUBLIC repository. Anyone on the internet will be able to see its code.",
                            form.slug()
                        ),
                        yes: "Publish publicly".into(),
                        action: Action::Publish { path, confirmed: true },
                    });
                    return;
                }
                self.progress.remove(&path);
                let label = if info.github.is_some() { "Pushing" } else { "Uploading" };
                self.spawn(label, vec![path], &info.name.clone(), move |job| {
                    ops::publish(job, &info, &form, &options).map(drop)
                });
            }
            Action::ReviewBatch => {
                for info in self.checked_projects() {
                    if !self.scans.contains_key(&info.path) {
                        self.handle(Action::Rescan(info.path));
                    }
                }
                self.batch_review = true;
            }
            Action::PublishChecked => {
                self.batch_review = false;
                let Some(options) = self.publish_options() else { return };
                let items: Vec<(ProjectInfo, RepoForm)> = self
                    .checked_projects()
                    .into_iter()
                    .filter(|p| !self.busy(&p.path))
                    .filter_map(|p| {
                        let form = self.forms.get(&p.path)?.clone();
                        Some((p, form))
                    })
                    .collect();
                if items.is_empty() {
                    return;
                }
                for (info, _) in &items {
                    self.progress.remove(&info.path);
                }
                let paths = items.iter().map(|(info, _)| info.path.clone()).collect();
                let label = format!("Uploading {} projects", items.len());
                self.spawn(label, paths, "batch", move |job| ops::publish_many(job, items, &options));
            }
            Action::Apply { path, confirmed } => {
                let Some(info) = self.info(&path) else { return };
                let form = self.form_for(&info).clone();
                let going_public = form.remote.as_ref().is_some_and(|r| r.private) && !form.private;
                if !confirmed && going_public {
                    self.confirm = Some(Confirm {
                        title: "Make this repository public?".into(),
                        body: format!(
                            "github.com/{} and its whole history will become visible to everyone. Check it for secrets first.",
                            form.slug()
                        ),
                        yes: "Make public".into(),
                        action: Action::Apply { path, confirmed: true },
                    });
                    return;
                }
                self.spawn("Saving settings", vec![path], &info.name.clone(), move |job| {
                    ops::apply(job, &info, &form)
                });
            }
            Action::Prepare(path) => {
                let Some(info) = self.info(&path) else { return };
                let form = self.form_for(&info).clone();
                let scan = self.scans.get(&path).cloned();
                self.spawn("Claude: preparing", vec![path], &info.name.clone(), move |job| {
                    ops::prepare(job, &info, &form, scan.as_ref(), &model)
                });
            }
            Action::Suggest(path) => {
                let Some(info) = self.info(&path) else { return };
                self.spawn("Claude: describing", vec![path], &info.name.clone(), move |job| {
                    ops::suggest(job, &info, &model)
                });
            }
            Action::CommitMessage(path) => {
                let Some(info) = self.info(&path) else { return };
                self.spawn("Claude: commit message", vec![path], &info.name.clone(), move |job| {
                    ops::commit_message(job, &info, &model)
                });
            }
            Action::FixProblems(path) => {
                let (Some(info), Some(scan)) = (self.info(&path), self.scans.get(&path).cloned()) else { return };
                self.spawn("Claude: fixing flags", vec![path], &info.name.clone(), move |job| {
                    ops::fix_problems(job, &info, &scan, &model)
                });
            }
            Action::Ignore { path, paths } => {
                let Some(info) = self.info(&path) else { return };
                self.spawn("Ignoring files", vec![path], &info.name.clone(), move |job| {
                    ops::ignore(job, &info, &paths)
                });
            }
            Action::RunTasks { path, on_checked } => {
                let Some(form) = self.forms.get(&path) else { return };
                let chosen = chosen_tasks(form, &self.cfg.prompts);
                let Some(request) = tasks::combine(&chosen, &form.claude_prompt) else { return };
                let may_run_git = form.claude_may_run_git;
                let targets: Vec<ProjectInfo> = if on_checked {
                    self.checked_projects().into_iter().filter(|p| !self.busy(&p.path)).collect()
                } else {
                    self.info(&path).into_iter().collect()
                };
                if targets.is_empty() {
                    return;
                }
                let paths = targets.iter().map(|p| p.path.clone()).collect();
                let label = match targets.len() {
                    1 => "Claude: working".to_string(),
                    n => format!("Claude: {n} projects"),
                };
                let tag = if targets.len() == 1 { targets[0].name.clone() } else { "claude".into() };
                self.spawn(label, paths, &tag, move |job| {
                    ops::run_tasks_many(job, targets, &request, &model, may_run_git)
                });
            }
            Action::SavePrompt(path) => {
                let Some(form) = self.forms.get_mut(&path) else { return };
                let text = form.claude_prompt.trim().to_string();
                if text.is_empty() {
                    return;
                }
                let name = match form.prompt_name.trim() {
                    "" => tasks::default_name(&text),
                    name => name.to_string(),
                };
                form.claude_tasks.insert(tasks::saved_id(&name));
                form.claude_prompt.clear();
                form.prompt_name.clear();
                match self.cfg.prompts.iter_mut().find(|p| p.name == name) {
                    Some(saved) => saved.text = text,
                    None => self.cfg.prompts.push(SavedPrompt { name: name.clone(), text }),
                }
                self.cfg.save();
                self.toast(LogKind::Ok, format!("Saved the prompt \"{name}\""));
            }
            Action::DeletePrompt(name) => {
                self.cfg.prompts.retain(|p| p.name != name);
                self.cfg.save();
                let id = tasks::saved_id(&name);
                for form in self.forms.values_mut() {
                    form.claude_tasks.remove(&id);
                }
                self.toast(LogKind::Info, format!("Deleted the prompt \"{name}\""));
            }
            Action::CopyToChecked(path) => {
                let Some(from) = self.forms.get(&path).cloned() else { return };
                let mut copied = 0;
                for target in self.checked.iter().filter(|p| **p != path) {
                    if let Some(form) = self.forms.get_mut(target) {
                        form.copy_shared_settings(&from);
                        copied += 1;
                    }
                }
                self.toast(
                    LogKind::Ok,
                    format!("Copied visibility, features, license and collaborators to {copied} project(s)"),
                );
            }
            Action::DismissOutcome(path) => {
                self.progress.remove(&path);
            }
            Action::Open(target) => {
                let _ = Command::new("explorer").arg(target).spawn();
            }
            Action::CopyText(text) => {
                self.bus_ctx().copy_text(text);
                self.toast(LogKind::Info, "Link copied");
            }
            Action::Cancel(id) => {
                if let Some(running) = self.running.iter().find(|r| r.id == id) {
                    running.cancel.cancel();
                }
            }
        }
    }

    fn bus_ctx(&self) -> egui::Context {
        self.bus.ctx().clone()
    }

    // ------------------------------------------------------------ layout

    fn top_bar(&mut self, ui: &mut Ui, actions: &mut Vec<Action>) {
        ui.add_space(6.0);
        ui.horizontal(|ui| {
            ui.label(RichText::new("GitHub Uploader").size(20.0).strong());
            ui.separator();
            ui.label("Projects folder");
            let field = ui.add(egui::TextEdit::singleline(&mut self.cfg.root).desired_width(320.0));
            if field.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                actions.push(Action::ReloadProjects);
            }
            if ui.button("📂 Browse…").clicked() {
                actions.push(Action::BrowseRoot);
            }
            if ui.button("🔄 Reload").clicked() {
                actions.push(Action::ReloadProjects);
            }
            ui.separator();
            if ui
                .button("⚙ Settings")
                .on_hover_text("Claude model and defaults for new repositories")
                .clicked()
            {
                actions.push(Action::OpenSettings);
            }

            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| match &self.account {
                None => {
                    ui.label("Checking GitHub…");
                    ui.spinner();
                }
                Some(Ok(account)) => {
                    ui.label(RichText::new(format!("✔ Signed in as {}", account.login)).color(GREEN));
                }
                Some(Err(_)) => {
                    if ui.small_button("🔄").on_hover_text("Check again").clicked() {
                        actions.push(Action::RefreshAccount);
                    }
                    if ui.button("Sign in to GitHub").clicked() {
                        actions.push(Action::SignIn);
                    }
                    ui.label(RichText::new("Not signed in to GitHub").color(RED));
                }
            });
        });
        ui.add_space(4.0);

        if let Some(tools) = &self.tools {
            if tools.git.is_none() {
                banner(ui, RED, "git isn't installed or isn't on PATH. Install Git for Windows, then restart.");
            }
            if tools.gh.is_none() {
                banner(ui, RED, "The GitHub CLI (gh) isn't installed. Get it from cli.github.com, then restart.");
            }
            if tools.claude.is_none() {
                banner(ui, YELLOW, "Claude Code wasn't found, so the ✨ Claude features are turned off.");
            }
        }
        if let (Some(None), Some(account)) = (&self.identity, self.account()) {
            let text = format!(
                "git has no name or email set. Commits will use {} <{}> (GitHub's private address).",
                account.display_name(),
                account.noreply_email()
            );
            ui.horizontal(|ui| {
                banner(ui, YELLOW, &text);
                if ui.button("Save as my git identity").clicked() {
                    actions.push(Action::SaveIdentity);
                }
            });
            ui.add_space(4.0);
        }
    }

    fn project_list(&mut self, ui: &mut Ui, actions: &mut Vec<Action>) {
        egui::Panel::bottom("batch").show(ui, |ui| {
            ui.add_space(6.0);
            let count = self.checked_projects().len();
            let signed_in = self.account().is_some();
            let upload = egui::Button::new(RichText::new(format!("🚀 Upload checked ({count})")).strong())
                .min_size(vec2(ui.available_width(), 32.0));
            if ui
                .add_enabled(count > 0 && signed_in, upload)
                .on_hover_text("Review the checked projects, then upload them one after another")
                .on_disabled_hover_text("Tick the projects you want to upload")
                .clicked()
            {
                actions.push(Action::ReviewBatch);
            }
            if let Some(path) = &self.selected {
                let copy = egui::Button::new("Copy this project's settings to checked")
                    .min_size(vec2(ui.available_width(), 26.0));
                if ui
                    .add_enabled(count > 0, copy)
                    .on_hover_text("Copies visibility, features, license, collaborators and the Claude option")
                    .clicked()
                {
                    actions.push(Action::CopyToChecked(path.clone()));
                }
            }
            ui.add_space(6.0);
        });

        ui.add_space(6.0);
        ui.horizontal(|ui| {
            ui.label(RichText::new("Projects").size(17.0).strong());
            ui.label(RichText::new(format!("{}", self.projects.len())).weak());
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let selectable: Vec<&PathBuf> =
                    self.projects.iter().filter(|p| !p.empty).map(|p| &p.path).collect();
                let mut all = !selectable.is_empty() && selectable.iter().all(|p| self.checked.contains(*p));
                if ui.checkbox(&mut all, "All").changed() {
                    self.checked = if all { selectable.into_iter().cloned().collect() } else { HashSet::new() };
                }
            });
        });
        ui.separator();

        let loading = self.projects.is_empty() && self.running.iter().any(|r| r.label == "Finding projects");
        if loading {
            ui.spinner();
        } else if self.projects.is_empty() {
            ui.label(RichText::new("No project folders here.").weak());
        }

        egui::ScrollArea::vertical().auto_shrink(false).show(ui, |ui| {
            let groups: [(&str, fn(&ProjectInfo) -> bool); 3] = [
                ("NOT ON GITHUB YET", |p| !p.empty && p.github.is_none()),
                ("ON GITHUB", |p| p.github.is_some()),
                ("EMPTY FOLDERS", |p| p.empty),
            ];
            for (title, belongs) in groups {
                let members: Vec<&ProjectInfo> = self.projects.iter().filter(|p| belongs(p)).collect();
                if members.is_empty() {
                    continue;
                }
                ui.add_space(4.0);
                ui.label(RichText::new(format!("{title}  {}", members.len())).small().strong().weak());
                for info in members {
                    let row = Row {
                        selected: self.selected.as_ref() == Some(&info.path),
                        busy: self.running.iter().any(|r| r.paths.contains(&info.path)),
                    };
                    project_row(ui, info, self.forms.get(&info.path), &row, &mut self.checked, actions);
                }
                ui.add_space(6.0);
            }
        });
    }

    fn details(&mut self, ui: &mut Ui, actions: &mut Vec<Action>) {
        let Some(info) = self.selected.as_ref().and_then(|p| self.info(p)) else {
            welcome(ui);
            return;
        };
        let path = info.path.clone();
        let view = View {
            busy: self.busy(&path),
            claude: self.claude_ready(),
            scanning: self.scanning.contains(&path),
            loading_remote: self.remote_loading.contains(&path),
            running: self
                .running
                .iter()
                .filter(|r| r.blocking && r.paths.contains(&path))
                .map(|r| (r.id, r.label.clone()))
                .collect(),
            checked: self.checked_projects().len(),
        };
        let account = self.account.as_ref().and_then(|a| a.as_ref().ok());
        let scan = self.scans.get(&path);
        let progress = self.progress.get(&path);
        let prompts = &self.cfg.prompts;
        let Some(form) = self.forms.get_mut(&path) else { return };

        egui::ScrollArea::vertical().auto_shrink(false).show(ui, |ui| {
            ui.add_space(8.0);
            header(ui, &info, actions);
            ui.add_space(10.0);
            ui.columns(2, |columns| {
                section(&mut columns[0], "Repository", |ui| repo_section(ui, &info, form, account, &view, actions));
                section(&mut columns[0], "Collaborators", |ui| collaborators_section(ui, &info, form, account));
                section(&mut columns[0], "Pre-flight check", |ui| {
                    preflight_section(ui, &info, scan, &view, actions)
                });
                let upload_title = if info.github.is_some() { "Push changes" } else { "Upload" };
                section(&mut columns[1], upload_title, |ui| {
                    upload_section(ui, &info, form, scan, account, progress, &view, actions)
                });
                section(&mut columns[1], "Claude", |ui| claude_section(ui, &info, form, prompts, &view, actions));
            });
        });
    }

    fn log_panel(&mut self, ui: &mut Ui, actions: &mut Vec<Action>) {
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            ui.label(RichText::new("Activity").strong());
            for running in &self.running {
                egui::Frame::new()
                    .fill(ui.visuals().faint_bg_color)
                    .corner_radius(10.0)
                    .inner_margin(egui::Margin::symmetric(8, 2))
                    .show(ui, |ui| {
                        ui.horizontal(|ui| {
                            ui.spinner();
                            let secs = running.started.elapsed().as_secs();
                            ui.label(RichText::new(format!("{} · {secs}s", running.label)).small());
                            if ui.small_button("✖").on_hover_text("Cancel").clicked() {
                                actions.push(Action::Cancel(running.id));
                            }
                        });
                    });
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.small_button("Clear").clicked() {
                    self.log.clear();
                }
            });
        });
        ui.separator();
        let weak = ui.visuals().weak_text_color();
        let normal = ui.visuals().text_color();
        egui::ScrollArea::vertical()
            .auto_shrink(false)
            .stick_to_bottom(true)
            .show(ui, |ui| {
                ui.spacing_mut().item_spacing.y = 2.0;
                for line in &self.log {
                    let font = egui::FontId::monospace(12.5);
                    let mut job = LayoutJob::default();
                    let tag: String = line.tag.chars().take(16).collect();
                    job.append(&format!("{tag:>16}  "), 0.0, TextFormat::simple(font.clone(), weak));
                    job.append(&line.text, 0.0, TextFormat::simple(font, kind_color(line.kind, normal, weak)));
                    ui.label(job);
                }
            });
    }

    fn toasts_ui(&mut self, ctx: &egui::Context, bottom_gap: f32) {
        let now = Instant::now();
        self.toasts.retain(|t| t.until > now);
        if self.toasts.is_empty() {
            return;
        }
        let mut dismissed = None;
        egui::Area::new(egui::Id::new("toasts"))
            .anchor(egui::Align2::RIGHT_BOTTOM, vec2(-18.0, -(bottom_gap + 14.0)))
            .order(egui::Order::Foreground)
            .show(ctx, |ui| {
                ui.set_max_width(420.0);
                for (i, toast) in self.toasts.iter().enumerate().rev().take(4) {
                    let color = kind_color(toast.kind, ui.visuals().text_color(), ui.visuals().weak_text_color());
                    let icon = match toast.kind {
                        LogKind::Ok => "✔",
                        LogKind::Warn => "⚠",
                        LogKind::Err => "✖",
                        _ => "ℹ",
                    };
                    let response = egui::Frame::new()
                        .fill(ui.visuals().window_fill)
                        .stroke(egui::Stroke::new(1.0, color.gamma_multiply(0.7)))
                        .corner_radius(8.0)
                        .inner_margin(egui::Margin::symmetric(14, 10))
                        .shadow(ui.visuals().popup_shadow)
                        .show(ui, |ui| {
                            ui.horizontal(|ui| {
                                ui.label(RichText::new(icon).color(color).size(16.0));
                                ui.label(RichText::new(&toast.text).color(color));
                            });
                        })
                        .response
                        .interact(egui::Sense::click())
                        .on_hover_text("Click to dismiss");
                    if response.clicked() {
                        dismissed = Some(i);
                    }
                    ui.add_space(6.0);
                }
            });
        if let Some(i) = dismissed {
            self.toasts.remove(i);
        }
        ctx.request_repaint_after(Duration::from_millis(500));
    }

    fn confirm_modal(&mut self, ctx: &egui::Context, actions: &mut Vec<Action>) {
        let Some(confirm) = &self.confirm else { return };
        let mut choice = None;
        let response = egui::Modal::new(egui::Id::new("confirm")).show(ctx, |ui| {
            ui.set_max_width(440.0);
            ui.label(RichText::new(&confirm.title).size(18.0).strong());
            ui.add_space(8.0);
            ui.label(&confirm.body);
            ui.add_space(14.0);
            ui.horizontal(|ui| {
                let yes = egui::Button::new(RichText::new(&confirm.yes).strong().color(Color32::WHITE))
                    .fill(RED.gamma_multiply(0.8));
                if ui.add(yes).clicked() {
                    choice = Some(true);
                }
                if ui.button("Cancel").clicked() {
                    choice = Some(false);
                }
            });
        });
        if response.should_close() && choice.is_none() {
            choice = Some(false);
        }
        match choice {
            Some(true) => actions.extend(self.confirm.take().map(|c| c.action)),
            Some(false) => self.confirm = None,
            None => {}
        }
    }

    fn settings_modal(&mut self, ctx: &egui::Context, actions: &mut Vec<Action>) {
        let Some(draft) = &mut self.settings else { return };
        let mut choice = None;
        let response = egui::Modal::new(egui::Id::new("settings")).show(ctx, |ui| {
            ui.set_width(460.0);
            ui.label(RichText::new("Settings").size(18.0).strong());
            ui.add_space(10.0);
            egui::Grid::new("settings-grid").num_columns(2).spacing([14.0, 10.0]).show(ui, |ui| {
                ui.label("Claude model");
                let current = MODELS.iter().find(|(v, _)| *v == draft.model).map_or("Custom", |(_, l)| l);
                egui::ComboBox::from_id_salt("model").selected_text(current).show_ui(ui, |ui| {
                    for (value, label) in MODELS {
                        ui.selectable_value(&mut draft.model, value.to_string(), label);
                    }
                });
                ui.end_row();

                ui.label(RichText::new("New repositories").strong());
                ui.end_row();

                ui.label("Start as");
                ui.horizontal(|ui| {
                    ui.selectable_value(&mut draft.private, true, "🔒 Private");
                    ui.selectable_value(&mut draft.private, false, "🌐 Public");
                });
                ui.end_row();

                ui.label("License");
                egui::ComboBox::from_id_salt("default-license")
                    .selected_text(draft.license.label())
                    .show_ui(ui, |ui| {
                        for license in License::ALL {
                            ui.selectable_value(&mut draft.license, license, license.label());
                        }
                    });
                ui.end_row();

                ui.label("Always invite");
                ui.add(
                    egui::TextEdit::singleline(&mut draft.collaborators)
                        .hint_text("GitHub usernames, comma-separated")
                        .desired_width(260.0),
                );
                ui.end_row();

                ui.label("Claude");
                ui.checkbox(&mut draft.claude_prepares, "Let Claude prepare repos before uploading");
                ui.end_row();
            });
            ui.add_space(8.0);
            ui.label(
                RichText::new("Saving also applies these to the projects that aren't on GitHub yet.")
                    .small()
                    .weak(),
            );
            ui.add_space(12.0);
            ui.horizontal(|ui| {
                let save = egui::Button::new(RichText::new("Save").strong().color(Color32::WHITE)).fill(ACCENT);
                if ui.add(save).clicked() {
                    choice = Some(true);
                }
                if ui.button("Cancel").clicked() {
                    choice = Some(false);
                }
            });
        });
        if response.should_close() && choice.is_none() {
            choice = Some(false);
        }
        match choice {
            Some(true) => actions.push(Action::SaveSettings),
            Some(false) => self.settings = None,
            None => {}
        }
    }

    fn batch_modal(&mut self, ctx: &egui::Context, actions: &mut Vec<Action>) {
        if !self.batch_review {
            return;
        }
        let items = self.checked_projects();
        let claude = self.claude_ready();
        let mut choice = None;
        let response = egui::Modal::new(egui::Id::new("batch")).show(ctx, |ui| {
            ui.set_min_width(620.0);
            ui.label(RichText::new(format!("Upload {} projects", items.len())).size(18.0).strong());
            ui.label(
                RichText::new("They go up one at a time. Anything that needs a look stops; the rest carry on.")
                    .small()
                    .weak(),
            );
            ui.add_space(10.0);
            egui::Grid::new("batch-grid").num_columns(4).striped(true).spacing([16.0, 8.0]).show(ui, |ui| {
                for heading in ["Project", "Repository", "Visibility", "Status"] {
                    ui.label(RichText::new(heading).small().strong().weak());
                }
                ui.end_row();
                for info in &items {
                    let Some(form) = self.forms.get_mut(&info.path) else { continue };
                    ui.label(RichText::new(&info.name).strong());
                    ui.label(format!("github.com/{}", info.slug().unwrap_or_else(|| form.slug())));
                    if info.github.is_none() {
                        ui.horizontal(|ui| {
                            ui.selectable_value(&mut form.private, true, "🔒 Private");
                            ui.selectable_value(&mut form.private, false, "🌐 Public");
                        });
                    } else {
                        let private = form.remote.as_ref().is_none_or(|r| r.private);
                        ui.label(RichText::new(if private { "🔒 private" } else { "🌐 public" }).weak());
                    }
                    let tidy = info.github.is_none() && form.claude_prepare && claude;
                    let (text, color) = batch_status(info, self.scans.get(&info.path), self.scanning.contains(&info.path), tidy, form.accept_warnings);
                    ui.label(RichText::new(text).color(color));
                    ui.end_row();
                }
            });
            ui.add_space(10.0);
            if claude && items.iter().any(|i| i.github.is_none()) {
                let new_forms: Vec<&PathBuf> = items.iter().filter(|i| i.github.is_none()).map(|i| &i.path).collect();
                let mut all = new_forms.iter().all(|p| self.forms.get(*p).is_some_and(|f| f.claude_prepare));
                if ui.checkbox(&mut all, "✨ Let Claude prepare new repos first (.gitignore, README, description)").changed() {
                    for path in new_forms {
                        if let Some(form) = self.forms.get_mut(path) {
                            form.claude_prepare = all;
                        }
                    }
                }
            }
            let public = items
                .iter()
                .filter(|i| i.github.is_none() && self.forms.get(&i.path).is_some_and(|f| !f.private))
                .count();
            if public > 0 {
                ui.label(RichText::new(format!("⚠ {public} will be public: anyone can see their code.")).color(YELLOW));
            }
            ui.add_space(12.0);
            ui.horizontal(|ui| {
                let go = egui::Button::new(
                    RichText::new(format!("🚀 Upload {} projects", items.len())).strong().color(Color32::WHITE),
                )
                .fill(ACCENT);
                if ui.add_enabled(!items.is_empty(), go).clicked() {
                    choice = Some(true);
                }
                if ui.button("Cancel").clicked() {
                    choice = Some(false);
                }
            });
        });
        if response.should_close() && choice.is_none() {
            choice = Some(false);
        }
        match choice {
            Some(true) => actions.push(Action::PublishChecked),
            Some(false) => self.batch_review = false,
            None => {}
        }
    }
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut Ui, _frame: &mut eframe::Frame) {
        self.drain();
        let mut actions = Vec::new();
        egui::Panel::top("top").show(ui, |ui| self.top_bar(ui, &mut actions));
        let log = egui::Panel::bottom("log")
            .resizable(true)
            .default_size(150.0)
            .min_size(70.0)
            .show(ui, |ui| self.log_panel(ui, &mut actions));
        egui::Panel::left("projects")
            .resizable(true)
            .default_size(270.0)
            .min_size(200.0)
            .show(ui, |ui| self.project_list(ui, &mut actions));
        egui::CentralPanel::default().show(ui, |ui| self.details(ui, &mut actions));
        let ctx = ui.ctx().clone();
        self.toasts_ui(&ctx, log.response.rect.height());
        self.confirm_modal(&ctx, &mut actions);
        self.settings_modal(&ctx, &mut actions);
        self.batch_modal(&ctx, &mut actions);
        for action in actions {
            self.handle(action);
        }
        if !self.running.is_empty() {
            // Keeps spinners and elapsed times moving.
            ctx.request_repaint_after(Duration::from_millis(250));
        }
    }
}

/// What the detail sections need to know besides the project and its form.
struct View {
    busy: bool,
    claude: bool,
    scanning: bool,
    loading_remote: bool,
    /// Blocking jobs on this project, for Cancel buttons.
    running: Vec<(u64, String)>,
    /// How many uploadable projects are ticked.
    checked: usize,
}

struct Row {
    selected: bool,
    busy: bool,
}

fn kind_color(kind: LogKind, normal: Color32, weak: Color32) -> Color32 {
    match kind {
        LogKind::Info => normal,
        LogKind::Cmd => BLUE,
        LogKind::Out => weak,
        LogKind::Ok => GREEN,
        LogKind::Warn => YELLOW,
        LogKind::Err => RED,
        LogKind::Claude => CLAUDE,
        LogKind::Tool => TEAL,
    }
}

fn project_row(
    ui: &mut Ui,
    info: &ProjectInfo,
    form: Option<&RepoForm>,
    row: &Row,
    checked: &mut HashSet<PathBuf>,
    actions: &mut Vec<Action>,
) {
    let fill = if row.selected { ui.visuals().selection.bg_fill.gamma_multiply(0.35) } else { Color32::TRANSPARENT };
    egui::Frame::new()
        .fill(fill)
        .corner_radius(6.0)
        .inner_margin(egui::Margin::symmetric(6, 5))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal(|ui| {
                let mut ticked = checked.contains(&info.path);
                if ui.add_enabled(!info.empty, egui::Checkbox::without_text(&mut ticked)).changed() {
                    if ticked {
                        checked.insert(info.path.clone());
                    } else {
                        checked.remove(&info.path);
                    }
                }
                if ui.selectable_label(row.selected, RichText::new(&info.name).strong()).clicked() {
                    actions.push(Action::Select(info.path.clone()));
                }
                if row.busy {
                    ui.spinner();
                }
            });
            let (text, color) = status_line(info, form);
            let status = egui::Label::new(RichText::new(text).small().color(color)).sense(egui::Sense::click());
            if ui.add(status).clicked() {
                actions.push(Action::Select(info.path.clone()));
            }
        });
}

fn status_line(info: &ProjectInfo, form: Option<&RepoForm>) -> (String, Color32) {
    if info.empty {
        return ("Nothing to upload".into(), Color32::GRAY);
    }
    if info.github.is_some() {
        let visibility = match form.and_then(|f| f.remote.as_ref()) {
            Some(remote) if remote.private => "🔒 private",
            Some(_) => "🌐 public",
            None => "on GitHub",
        };
        let changes = match info.changes {
            0 => String::new(),
            1 => " · 1 change".into(),
            n => format!(" · {n} changes"),
        };
        return (format!("{visibility}{changes}"), GREEN);
    }
    if info.origin.is_some() {
        return ("Has a non-GitHub remote".into(), YELLOW);
    }
    if info.is_git {
        return ("Local git repo".into(), BLUE);
    }
    ("Not a git repo yet".into(), Color32::GRAY)
}

fn batch_status(
    info: &ProjectInfo,
    scan: Option<&ScanReport>,
    scanning: bool,
    claude_tidies: bool,
    accepted: bool,
) -> (String, Color32) {
    let what = if info.github.is_some() { "push" } else { "new repo" };
    let (state, color) = match scan {
        _ if info.github.is_some() && info.changes == 0 => ("nothing new".to_string(), Color32::GRAY),
        None if scanning => ("checking files…".to_string(), Color32::GRAY),
        None => ("not checked yet".to_string(), Color32::GRAY),
        Some(report) if report.error.is_some() => ("can't read files".to_string(), RED),
        Some(_) if claude_tidies => ("✨ Claude tidies, then uploads".to_string(), CLAUDE),
        Some(report) if report.blocked() => ("⛔ files too big".to_string(), RED),
        Some(report) if report.warnings() > 0 && !accepted => (format!("⚠ {} to review", report.warnings()), YELLOW),
        Some(_) => ("✔ ready".to_string(), GREEN),
    };
    (format!("{what} · {state}"), color)
}

fn banner(ui: &mut Ui, color: Color32, text: &str) {
    egui::Frame::new()
        .fill(color.gamma_multiply(0.15))
        .stroke(egui::Stroke::new(1.0, color.gamma_multiply(0.6)))
        .corner_radius(6.0)
        .inner_margin(egui::Margin::symmetric(10, 6))
        .show(ui, |ui| {
            ui.label(RichText::new(text).color(color));
        });
}

fn section(ui: &mut Ui, title: &str, add: impl FnOnce(&mut Ui)) {
    egui::Frame::group(ui.style())
        .inner_margin(14.0)
        .corner_radius(8.0)
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.label(RichText::new(title).size(16.0).strong());
            ui.add_space(8.0);
            add(ui);
        });
    ui.add_space(12.0);
}

fn welcome(ui: &mut Ui) {
    ui.vertical_centered(|ui| {
        ui.add_space(ui.available_height() * 0.2);
        ui.label(RichText::new("Pick a project on the left").size(22.0).strong());
        ui.add_space(14.0);
        for step in [
            "1.  Name it, pick private or public, and add collaborators.",
            "2.  Press \"Prepare & upload\": Claude tidies the repo, then it goes up to GitHub.",
            "3.  Or pick any number of Claude tasks and run them on one project or all the ticked ones.",
            "4.  Tick several projects to upload them in one go.",
        ] {
            ui.label(RichText::new(step).size(15.0));
            ui.add_space(4.0);
        }
    });
}

fn chip(ui: &mut Ui, text: &str, color: Color32) {
    egui::Frame::new()
        .fill(color.gamma_multiply(0.14))
        .corner_radius(10.0)
        .inner_margin(egui::Margin::symmetric(8, 2))
        .show(ui, |ui| {
            ui.label(RichText::new(text).small().color(color));
        });
}

fn header(ui: &mut Ui, info: &ProjectInfo, actions: &mut Vec<Action>) {
    ui.horizontal(|ui| {
        ui.label(RichText::new(&info.name).size(24.0).strong());
        if let Some(slug) = info.slug() {
            ui.label(RichText::new(format!("github.com/{slug}")).color(GREEN));
        }
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if ui.button("🔄 Refresh").on_hover_text("Re-read the folder and GitHub").clicked() {
                actions.push(Action::Refresh(info.path.clone()));
            }
            if let Some(slug) = info.slug() {
                if ui.button("🌐 Open on GitHub").clicked() {
                    actions.push(Action::Open(format!("https://github.com/{slug}")));
                }
            }
            if ui.button("📂 Open folder").clicked() {
                actions.push(Action::Open(info.path.display().to_string()));
            }
        });
    });
    ui.label(RichText::new(info.path.display().to_string()).weak());
    ui.add_space(4.0);
    ui.horizontal_wrapped(|ui| {
        if info.is_git {
            let branch = info.branch.as_deref().unwrap_or("no branch");
            chip(ui, &format!("git · {branch}"), BLUE);
            match info.changes {
                0 => chip(ui, "no uncommitted changes", Color32::GRAY),
                n => chip(ui, &format!("{n} uncommitted change(s)"), YELLOW),
            }
        } else {
            chip(ui, "not a git repo yet", Color32::GRAY);
        }
        for (present, name) in [
            (info.has_readme, "README"),
            (info.has_gitignore, ".gitignore"),
            (info.has_license, "LICENSE"),
        ] {
            let (mark, color) = if present { ("✔", GREEN) } else { ("✖", Color32::GRAY) };
            chip(ui, &format!("{mark} {name}"), color);
        }
    });
}

fn repo_section(
    ui: &mut Ui,
    info: &ProjectInfo,
    form: &mut RepoForm,
    account: Option<&Account>,
    view: &View,
    actions: &mut Vec<Action>,
) {
    let existing = info.github.is_some();
    if existing {
        if view.loading_remote {
            ui.horizontal(|ui| {
                ui.spinner();
                ui.label("Loading settings from GitHub…");
            });
        } else if let Some(err) = &form.remote_error {
            ui.label(RichText::new(format!("Couldn't load GitHub settings: {err}")).color(RED));
        }
    }
    egui::Grid::new("repo").num_columns(2).spacing([14.0, 10.0]).show(ui, |ui| {
        ui.label("Name");
        if existing {
            ui.label(RichText::new(format!("{}/{}", form.owner, form.name)).strong());
        } else {
            ui.vertical(|ui| {
                ui.horizontal(|ui| {
                    ui.label(RichText::new(format!("github.com/{}/", form.owner)).weak());
                    ui.add(egui::TextEdit::singleline(&mut form.name).desired_width(f32::INFINITY));
                });
                let clean = project::repo_name(&form.name);
                if clean != form.name {
                    ui.label(RichText::new(format!("Will be created as “{clean}”")).small().weak());
                }
            });
        }
        ui.end_row();

        ui.label("Description");
        ui.horizontal(|ui| {
            let width = ui.available_width() - 96.0;
            ui.add(
                egui::TextEdit::singleline(&mut form.description)
                    .hint_text("What is this project? (optional)")
                    .desired_width(width),
            );
            let suggest = ui
                .add_enabled(view.claude && !view.busy, egui::Button::new("✨ Suggest"))
                .on_hover_text("Claude reads the project and suggests a description and topics");
            if suggest.clicked() {
                actions.push(Action::Suggest(info.path.clone()));
            }
        });
        ui.end_row();

        ui.label("Visibility");
        ui.vertical(|ui| {
            ui.horizontal(|ui| {
                ui.selectable_value(&mut form.private, true, RichText::new("🔒 Private").size(15.0));
                ui.selectable_value(&mut form.private, false, RichText::new("🌐 Public").size(15.0));
            });
            if form.private {
                ui.label(RichText::new("Only you and your collaborators can see it.").small().weak());
            } else {
                ui.label(RichText::new("Anyone on the internet will be able to see this code.").small().color(YELLOW));
            }
        });
        ui.end_row();
    });

    ui.add_space(6.0);
    egui::CollapsingHeader::new("More options").id_salt("more-options").show(ui, |ui| {
        egui::Grid::new("repo-more").num_columns(2).spacing([14.0, 10.0]).show(ui, |ui| {
            if !existing {
                ui.label("Owner");
                egui::ComboBox::from_id_salt("owner")
                    .selected_text(if form.owner.is_empty() { "—" } else { form.owner.as_str() })
                    .show_ui(ui, |ui| {
                        if let Some(account) = account {
                            let you = format!("{} (you)", account.login);
                            ui.selectable_value(&mut form.owner, account.login.clone(), you);
                            for org in &account.orgs {
                                ui.selectable_value(&mut form.owner, org.clone(), org.as_str());
                            }
                        }
                    });
                ui.end_row();
            }

            ui.label("Website");
            ui.add(
                egui::TextEdit::singleline(&mut form.homepage)
                    .hint_text("https://… (optional)")
                    .desired_width(f32::INFINITY),
            );
            ui.end_row();

            ui.label("Topics");
            ui.add(
                egui::TextEdit::singleline(&mut form.topics)
                    .hint_text("rust, gui, tools")
                    .desired_width(f32::INFINITY),
            );
            ui.end_row();

            ui.label("Features");
            ui.horizontal_wrapped(|ui| {
                ui.checkbox(&mut form.issues, "Issues");
                ui.checkbox(&mut form.wiki, "Wiki");
                ui.checkbox(&mut form.projects, "Projects");
                ui.checkbox(&mut form.discussions, "Discussions");
            });
            ui.end_row();

            ui.label("License");
            if info.has_license {
                ui.label(RichText::new("This project already has a license file.").weak());
            } else {
                egui::ComboBox::from_id_salt("license")
                    .selected_text(form.license.label())
                    .show_ui(ui, |ui| {
                        for license in License::ALL {
                            ui.selectable_value(&mut form.license, license, license.label());
                        }
                    });
            }
            ui.end_row();
        });
    });
}

fn clean_login(text: &str) -> String {
    text.trim().trim_start_matches('@').to_string()
}

fn valid_login(login: &str) -> bool {
    !login.is_empty()
        && login.len() <= 39
        && login.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
        && !login.starts_with('-')
}

fn perm_picker(ui: &mut Ui, id: impl std::hash::Hash + std::fmt::Debug, perm: &mut Perm) {
    egui::ComboBox::from_id_salt(id)
        .selected_text(perm.label())
        .width(96.0)
        .show_ui(ui, |ui| {
            for p in Perm::ALL {
                ui.selectable_value(perm, p, p.label());
            }
        });
}

fn collaborators_section(ui: &mut Ui, info: &ProjectInfo, form: &mut RepoForm, account: Option<&Account>) {
    let existing = info.github.is_some();
    if form.collaborators.is_empty() {
        ui.label(RichText::new("Just you so far. Add people by their GitHub username.").weak());
    } else {
        let mut drop_new = None;
        egui::Grid::new("collaborators").num_columns(4).spacing([12.0, 6.0]).show(ui, |ui| {
            for (i, c) in form.collaborators.iter_mut().enumerate() {
                let name = RichText::new(format!("👤 {}", c.login)).strong();
                ui.label(if c.remove { name.strikethrough().weak() } else { name });
                ui.add_enabled_ui(!c.remove, |ui| perm_picker(ui, ("perm", i), &mut c.perm));
                let (status, color) = collab_status(c);
                ui.label(RichText::new(status).small().color(color));
                if c.kind == CollabKind::New {
                    if ui.small_button("✖").on_hover_text("Don't add").clicked() {
                        drop_new = Some(i);
                    }
                } else if ui.small_button(if c.remove { "Undo" } else { "Remove" }).clicked() {
                    c.remove = !c.remove;
                }
                ui.end_row();
            }
        });
        if let Some(i) = drop_new {
            form.collaborators.remove(i);
        }
    }

    ui.add_space(8.0);
    ui.horizontal(|ui| {
        let field = ui.add(
            egui::TextEdit::singleline(&mut form.new_collaborator)
                .hint_text("GitHub username")
                .desired_width(180.0),
        );
        perm_picker(ui, "new-perm", &mut form.new_perm);
        let login = clean_login(&form.new_collaborator);
        let duplicate = form.collaborators.iter().any(|c| c.login.eq_ignore_ascii_case(&login));
        let is_owner = login.eq_ignore_ascii_case(&form.owner);
        let ok = valid_login(&login) && !duplicate && !is_owner;
        let enter = field.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
        let add = ui.add_enabled(ok, egui::Button::new("➕ Add")).clicked();
        if ok && (add || enter) {
            form.collaborators.push(Collab::new(&login, form.new_perm));
            form.new_collaborator.clear();
        }
        if !login.is_empty() && !ok {
            let why = if duplicate {
                "Already in the list"
            } else if is_owner {
                "That's the owner"
            } else {
                "Not a valid GitHub username"
            };
            ui.label(RichText::new(why).small().color(RED));
        }
    });
    ui.add_space(4.0);
    let personal = account.is_some_and(|a| a.login.eq_ignore_ascii_case(&form.owner));
    let mut note = if existing {
        "Changes go to GitHub when you press Save settings.".to_string()
    } else {
        "Invitations go out as soon as the repository is created.".to_string()
    };
    if personal {
        note += " On personal repos everyone invited gets write access; levels only apply to organization repos.";
    }
    ui.label(RichText::new(note).small().weak());
}

fn collab_status(c: &Collab) -> (&'static str, Color32) {
    match (c.kind, c.remove) {
        (_, true) => ("will be removed", RED),
        (CollabKind::New, false) => ("will be invited", BLUE),
        (_, false) if c.perm != c.saved_perm => ("permission change pending", YELLOW),
        (CollabKind::Member, false) => ("collaborator", GREEN),
        (CollabKind::Invited(_), false) => ("invitation pending", YELLOW),
    }
}

/// One group of flagged paths, each with its own Ignore button when it can be ignored.
fn problem_list(
    ui: &mut Ui,
    info: &ProjectInfo,
    id: &str,
    title: RichText,
    items: Vec<(String, String, Option<bool>)>,
    view: &View,
    actions: &mut Vec<Action>,
) {
    if items.is_empty() {
        return;
    }
    let open = items.len() <= 6;
    egui::CollapsingHeader::new(title).id_salt(id).default_open(open).show(ui, |ui| {
        for (path, detail, folder) in items.iter().take(60) {
            ui.horizontal(|ui| {
                ui.label(RichText::new(path).monospace().small());
                ui.label(RichText::new(detail).small().weak());
                if let Some(folder) = folder {
                    let ignore = ui
                        .add_enabled(!view.busy, egui::Button::new(RichText::new("Ignore").small()).small())
                        .on_hover_text(format!(
                            "Add {} to .gitignore so it stays off GitHub",
                            project::ignore_rule(path, *folder)
                        ));
                    if ignore.clicked() {
                        actions.push(Action::Ignore {
                            path: info.path.clone(),
                            paths: vec![(path.clone(), *folder)],
                        });
                    }
                }
            });
        }
        if items.len() > 60 {
            ui.label(RichText::new(format!("…and {} more", items.len() - 60)).weak());
        }
    });
}

fn preflight_section(ui: &mut Ui, info: &ProjectInfo, scan: Option<&ScanReport>, view: &View, actions: &mut Vec<Action>) {
    let Some(report) = scan else {
        ui.horizontal(|ui| {
            if view.scanning {
                ui.spinner();
                ui.label("Checking the project's files…");
            } else if ui.button("Check files").clicked() {
                actions.push(Action::Rescan(info.path.clone()));
            }
        });
        return;
    };
    if let Some(err) = &report.error {
        ui.label(RichText::new(format!("Couldn't list the files: {err}")).color(RED));
    } else {
        let how = if report.via_git { "tracked plus new, unignored files" } else { "after applying .gitignore" };
        ui.label(format!("{} files · {} would be uploaded", report.files, size_text(report.bytes)));
        ui.label(RichText::new(format!("({how})")).small().weak());
        ui.add_space(6.0);
        if report.too_big.is_empty() && report.warnings() == 0 {
            ui.label(RichText::new("✔ Nothing looks out of place").color(GREEN));
        }
        let sized = |items: &[(String, u64)]| -> Vec<(String, String, Option<bool>)> {
            items.iter().map(|(path, size)| (path.clone(), size_text(*size), Some(false))).collect()
        };
        problem_list(
            ui,
            info,
            "too-big",
            RichText::new(format!("⛔ {} file(s) over GitHub's 100 MB limit", report.too_big.len())).color(RED),
            sized(&report.too_big),
            view,
            actions,
        );
        problem_list(
            ui,
            info,
            "secret-hits",
            RichText::new(format!("⚠ {} file(s) seem to contain keys or tokens", report.secret_hits.len())).color(YELLOW),
            report
                .secret_hits
                .iter()
                .map(|(path, what)| (path.clone(), what.to_string(), None))
                .collect(),
            view,
            actions,
        );
        problem_list(
            ui,
            info,
            "secret-files",
            RichText::new(format!("⚠ {} file(s) that usually hold secrets", report.secret_files.len())).color(YELLOW),
            report.secret_files.iter().map(|path| (path.clone(), String::new(), Some(false))).collect(),
            view,
            actions,
        );
        problem_list(
            ui,
            info,
            "heavy",
            RichText::new(format!("⚠ {} dependency or build folder(s) not ignored", report.heavy_dirs.len()))
                .color(YELLOW),
            report
                .heavy_dirs
                .iter()
                .map(|(dir, files, size)| (dir.clone(), format!("{files} files, {}", size_text(*size)), Some(true)))
                .collect(),
            view,
            actions,
        );
        problem_list(
            ui,
            info,
            "large",
            RichText::new(format!("⚠ {} large file(s), 50–100 MB", report.large.len())).color(YELLOW),
            sized(&report.large),
            view,
            actions,
        );
        egui::CollapsingHeader::new("What's included").id_salt("included").show(ui, |ui| {
            egui::Grid::new("top-level").num_columns(3).striped(true).show(ui, |ui| {
                for (entry, files, size) in report.top_level.iter().take(40) {
                    ui.label(RichText::new(entry).monospace());
                    ui.label(RichText::new(format!("{files} files")).weak());
                    ui.label(size_text(*size));
                    ui.end_row();
                }
            });
        });
    }

    ui.add_space(8.0);
    ui.horizontal_wrapped(|ui| {
        if ui.add_enabled(!view.busy && !view.scanning, egui::Button::new("🔄 Check again")).clicked() {
            actions.push(Action::Rescan(info.path.clone()));
        }
        let ignorable = report.ignorable();
        if !ignorable.is_empty() {
            let ignore_all = ui
                .add_enabled(!view.busy, egui::Button::new(format!("🙈 Ignore all {}", ignorable.len())))
                .on_hover_text("Add every flagged file and folder above to .gitignore");
            if ignore_all.clicked() {
                actions.push(Action::Ignore {
                    path: info.path.clone(),
                    paths: ignorable,
                });
            }
        }
        if (!report.too_big.is_empty() || report.warnings() > 0) && view.claude {
            let fix = ui
                .add_enabled(!view.busy, egui::Button::new("✨ Fix with Claude"))
                .on_hover_text("Claude adds .gitignore rules, untracks files and moves keys out of the code");
            if fix.clicked() {
                actions.push(Action::FixProblems(info.path.clone()));
            }
        }
    });
}

#[derive(Clone, Copy)]
enum Mark {
    Good,
    Todo,
    Warn,
    Bad,
    Claude,
}

fn check_row(ui: &mut Ui, mark: Mark, text: impl Into<String>, extra: impl FnOnce(&mut Ui)) {
    let (icon, color) = match mark {
        Mark::Good => ("✔", GREEN),
        Mark::Todo => ("○", Color32::GRAY),
        Mark::Warn => ("⚠", YELLOW),
        Mark::Bad => ("⛔", RED),
        Mark::Claude => ("✨", CLAUDE),
    };
    ui.horizontal(|ui| {
        ui.add_sized([18.0, 18.0], egui::Label::new(RichText::new(icon).color(color)));
        ui.label(text.into());
        extra(ui);
    });
}

#[allow(clippy::too_many_arguments)]
fn upload_section(
    ui: &mut Ui,
    info: &ProjectInfo,
    form: &mut RepoForm,
    scan: Option<&ScanReport>,
    account: Option<&Account>,
    progress: Option<&Progress>,
    view: &View,
    actions: &mut Vec<Action>,
) {
    let new = info.github.is_none();
    let claude_tidies = new && form.claude_prepare && view.claude;
    let path = info.path.clone();

    // What's ready, and what will happen.
    match account {
        Some(account) => check_row(ui, Mark::Good, format!("Signed in as {}", account.login), |_| {}),
        None => check_row(ui, Mark::Bad, "Not signed in to GitHub", |ui| {
            if ui.small_button("Sign in").clicked() {
                actions.push(Action::SignIn);
            }
        }),
    }
    match scan {
        None if view.scanning => {
            ui.horizontal(|ui| {
                ui.add_sized([18.0, 18.0], egui::Spinner::new());
                ui.label("Checking files…");
            });
        }
        None => check_row(ui, Mark::Todo, "Files not checked yet", |ui| {
            if ui.small_button("Check").clicked() {
                actions.push(Action::Rescan(path.clone()));
            }
        }),
        Some(report) if report.error.is_some() => check_row(ui, Mark::Bad, "Couldn't read the files", |_| {}),
        Some(report) if report.too_big.is_empty() && report.warnings() == 0 => check_row(
            ui,
            Mark::Good,
            format!("{} files, {}, nothing out of place", report.files, size_text(report.bytes)),
            |_| {},
        ),
        Some(report) => {
            let flagged = report.too_big.len() + report.warnings();
            let mark = if claude_tidies {
                Mark::Claude
            } else if report.blocked() {
                Mark::Bad
            } else {
                Mark::Warn
            };
            let text = if claude_tidies {
                format!("{flagged} flagged item(s): Claude will try to fix them")
            } else {
                format!("{flagged} flagged item(s) need a look (see Pre-flight check)")
            };
            check_row(ui, mark, text, |ui| {
                let ignorable = report.ignorable();
                if !ignorable.is_empty() && ui.add_enabled(!view.busy, egui::Button::new("Ignore all").small()).clicked() {
                    actions.push(Action::Ignore {
                        path: path.clone(),
                        paths: ignorable,
                    });
                }
            });
        }
    }
    if new {
        let readme = if info.has_readme {
            (Mark::Good, "Has a README")
        } else if claude_tidies {
            (Mark::Claude, "No README yet: Claude will write one")
        } else {
            (Mark::Todo, "No README (optional)")
        };
        check_row(ui, readme.0, readme.1, |_| {});
        let gitignore = if info.has_gitignore {
            (Mark::Good, "Has a .gitignore")
        } else if claude_tidies {
            (Mark::Claude, "No .gitignore yet: Claude will add one")
        } else {
            (Mark::Todo, "No .gitignore (optional)")
        };
        check_row(ui, gitignore.0, gitignore.1, |_| {});
        let description = if !form.description.trim().is_empty() {
            (Mark::Good, "Has a description")
        } else if claude_tidies {
            (Mark::Claude, "No description yet: Claude will write one")
        } else {
            (Mark::Todo, "No description (optional)")
        };
        check_row(ui, description.0, description.1, |_| {});
        let invites = form.collaborators.iter().filter(|c| !c.remove).count();
        if invites > 0 {
            check_row(ui, Mark::Good, format!("{invites} collaborator(s) will be invited"), |_| {});
        }
    } else {
        match info.changes {
            0 => check_row(ui, Mark::Good, "Everything is committed", |_| {}),
            n => check_row(ui, Mark::Todo, format!("{n} uncommitted change(s) to push"), |_| {}),
        }
        if form.has_unapplied_changes() {
            check_row(ui, Mark::Warn, "Settings changed here but not on GitHub yet", |_| {});
        }
    }

    ui.add_space(8.0);
    if new && view.claude {
        ui.checkbox(&mut form.claude_prepare, RichText::new("✨ Let Claude prepare it first").color(CLAUDE))
            .on_hover_text(
                "Before uploading, Claude makes sure .gitignore keeps junk and secrets out, writes a README if \
                 there isn't one, and fills in the description and topics.",
            );
    }
    let flagged = scan.is_some_and(|r| r.warnings() > 0);
    if flagged && !claude_tidies {
        ui.checkbox(&mut form.accept_warnings, "I've reviewed the flagged files, upload them anyway");
    }

    let commit_box = |ui: &mut Ui, form: &mut RepoForm, actions: &mut Vec<Action>| {
        ui.horizontal(|ui| {
            let width = ui.available_width() - 104.0;
            let hint = if new { "Initial commit" } else { "Describe your changes" };
            ui.add(
                egui::TextEdit::multiline(&mut form.commit_message)
                    .hint_text(hint)
                    .desired_rows(2)
                    .desired_width(width),
            );
            let write = ui
                .add_enabled(view.claude && !view.busy, egui::Button::new("✨ Write it"))
                .on_hover_text("Claude writes a commit message from your changes");
            if write.clicked() {
                actions.push(Action::CommitMessage(path.clone()));
            }
        });
    };
    if new {
        let summary = form.commit_message.lines().next().unwrap_or("Initial commit").to_string();
        egui::CollapsingHeader::new(format!("Commit message: {summary}"))
            .id_salt("commit-message")
            .show(ui, |ui| commit_box(ui, form, actions));
    } else {
        ui.label("Commit message");
        commit_box(ui, form, actions);
    }

    let mut blockers = Vec::new();
    if account.is_none() {
        blockers.push("Sign in to GitHub first.");
    }
    if info.empty {
        blockers.push("This folder is empty.");
    }
    if new && info.origin.is_some() {
        blockers.push("This repo has an origin remote that isn't GitHub.");
    }
    if new && form.owner.is_empty() {
        blockers.push("Pick an owner under More options.");
    }
    if let Some(report) = scan.filter(|_| !claude_tidies) {
        if report.blocked() {
            blockers.push("Ignore the files over 100 MB first.");
        } else if report.warnings() > 0 && !form.accept_warnings {
            blockers.push("Ignore the flagged files, or tick \"upload them anyway\".");
        }
    }
    if view.busy {
        blockers.push("Working on it…");
    }

    ui.add_space(8.0);
    ui.horizontal(|ui| {
        let label = match (new, claude_tidies) {
            (true, true) => "🚀 Prepare & upload",
            (true, false) => "🚀 Upload to GitHub",
            (false, _) => "⬆ Commit & push",
        };
        let ready = blockers.is_empty();
        let button = egui::Button::new(RichText::new(label).size(16.0).strong().color(Color32::WHITE))
            .fill(if ready { ACCENT } else { ACCENT.gamma_multiply(0.3) })
            .min_size(vec2(230.0, 38.0));
        if ui.add_enabled(ready, button).clicked() {
            actions.push(Action::Publish {
                path: path.clone(),
                confirmed: false,
            });
        }
        if !new {
            let save = egui::Button::new("💾 Save settings").min_size(vec2(140.0, 38.0));
            let response = ui
                .add_enabled(form.has_unapplied_changes() && !view.busy && account.is_some(), save)
                .on_hover_text("Send the repository settings and collaborator changes to GitHub");
            if response.clicked() {
                actions.push(Action::Apply {
                    path: path.clone(),
                    confirmed: false,
                });
            }
        }
    });
    for blocker in blockers.iter().filter(|_| !view.busy) {
        ui.label(RichText::new(format!("• {blocker}")).small().weak());
    }

    if let Some(progress) = progress {
        ui.add_space(10.0);
        progress_view(ui, &path, progress, view, actions);
    }
}

fn progress_view(ui: &mut Ui, path: &Path, progress: &Progress, view: &View, actions: &mut Vec<Action>) {
    for step in &progress.steps {
        ui.horizontal(|ui| {
            let (icon, color) = match step.state {
                StepState::Pending => ("○", Color32::GRAY),
                StepState::Running => ("", Color32::WHITE),
                StepState::Done => ("✔", GREEN),
                StepState::Attention => ("⚠", YELLOW),
                StepState::Failed => ("✖", RED),
            };
            if step.state == StepState::Running {
                ui.add_sized([18.0, 18.0], egui::Spinner::new());
            } else {
                ui.add_sized([18.0, 18.0], egui::Label::new(RichText::new(icon).color(color)));
            }
            let text = RichText::new(&step.label);
            ui.label(if step.state == StepState::Pending { text.weak() } else { text });
            if let Some(note) = &step.note {
                ui.label(RichText::new(note).small().weak());
            }
        });
    }
    if progress.outcome.is_none() {
        for (id, _) in &view.running {
            if ui.small_button("Cancel").clicked() {
                actions.push(Action::Cancel(*id));
            }
        }
    }
    let Some(outcome) = &progress.outcome else { return };
    let (icon, color) = match outcome.kind {
        OutcomeKind::Success => ("🎉", GREEN),
        OutcomeKind::Attention => ("⚠", YELLOW),
        OutcomeKind::Failed => ("✖", RED),
    };
    ui.add_space(6.0);
    egui::Frame::new()
        .fill(color.gamma_multiply(0.12))
        .stroke(egui::Stroke::new(1.0, color.gamma_multiply(0.5)))
        .corner_radius(8.0)
        .inner_margin(egui::Margin::same(12))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal(|ui| {
                ui.label(RichText::new(icon).size(18.0).color(color));
                ui.add(egui::Label::new(RichText::new(&outcome.text).color(color)).wrap());
            });
            ui.horizontal(|ui| {
                if let Some(url) = &outcome.url {
                    if ui.button("🌐 Open on GitHub").clicked() {
                        actions.push(Action::Open(url.clone()));
                    }
                    if ui.button("📋 Copy link").clicked() {
                        actions.push(Action::CopyText(url.clone()));
                    }
                }
                if ui.small_button("Dismiss").clicked() {
                    actions.push(Action::DismissOutcome(path.to_path_buf()));
                }
            });
        });
}

/// The selected tasks as `(label, prompt)`, built-in ones first.
fn chosen_tasks(form: &RepoForm, saved: &[SavedPrompt]) -> Vec<(String, String)> {
    let built_in = tasks::BUILT_IN
        .iter()
        .filter(|t| form.claude_tasks.contains(t.id))
        .map(|t| (t.label.to_string(), t.prompt.to_string()));
    let own = saved
        .iter()
        .filter(|p| form.claude_tasks.contains(&tasks::saved_id(&p.name)))
        .map(|p| (p.name.clone(), p.text.clone()));
    built_in.chain(own).collect()
}

fn task_chip(ui: &mut Ui, form: &mut RepoForm, id: String, label: &str, tooltip: &str) -> egui::Response {
    let selected = form.claude_tasks.contains(&id);
    let text = if selected { format!("✔ {label}") } else { label.to_string() };
    let response = ui.add(egui::Button::selectable(selected, text)).on_hover_text(tooltip);
    if response.clicked() {
        if selected {
            form.claude_tasks.remove(&id);
        } else {
            form.claude_tasks.insert(id);
        }
    }
    response
}

fn claude_section(
    ui: &mut Ui,
    info: &ProjectInfo,
    form: &mut RepoForm,
    saved: &[SavedPrompt],
    view: &View,
    actions: &mut Vec<Action>,
) {
    if !view.claude {
        ui.label(RichText::new("Claude Code wasn't found, so these features are off.").weak());
        return;
    }
    ui.horizontal(|ui| {
        let prepare = egui::Button::new(RichText::new("✨ Prepare for GitHub").strong().color(CLAUDE));
        let clicked = ui
            .add_enabled(!view.busy, prepare)
            .on_hover_text("Fixes .gitignore, writes a README if missing, and fills in the description and topics")
            .clicked();
        if clicked {
            actions.push(Action::Prepare(info.path.clone()));
        }
        ui.label(RichText::new("without uploading").small().weak());
    });

    ui.add_space(10.0);
    ui.label(RichText::new("Pick any number of tasks").strong());
    ui.horizontal_wrapped(|ui| {
        for task in tasks::BUILT_IN {
            task_chip(ui, form, task.id.to_string(), task.label, task.prompt);
        }
    });
    ui.add_space(6.0);
    ui.label(RichText::new("Your prompts").strong());
    if saved.is_empty() {
        ui.label(
            RichText::new("None yet. Write instructions below and press Save to keep them as a chip.")
                .small()
                .weak(),
        );
    } else {
        ui.horizontal_wrapped(|ui| {
            for prompt in saved {
                let tooltip = format!("{}\n\nRight-click to delete.", prompt.text);
                let chip = task_chip(ui, form, tasks::saved_id(&prompt.name), &prompt.name, &tooltip);
                chip.context_menu(|ui| {
                    if ui.button("🗑 Delete this prompt").clicked() {
                        actions.push(Action::DeletePrompt(prompt.name.clone()));
                        ui.close();
                    }
                });
            }
        });
    }

    ui.add_space(8.0);
    let editor = ui.add(
        egui::TextEdit::multiline(&mut form.claude_prompt)
            .hint_text("Extra instructions (optional), e.g. “Add a screenshots section to the README”")
            .desired_rows(3)
            .desired_width(f32::INFINITY),
    );
    let ctrl_enter = editor.has_focus() && ui.input(|i| i.modifiers.ctrl && i.key_pressed(egui::Key::Enter));
    ui.horizontal(|ui| {
        let has_text = !form.claude_prompt.trim().is_empty();
        ui.add_enabled(
            has_text,
            egui::TextEdit::singleline(&mut form.prompt_name)
                .hint_text("Name (optional)")
                .desired_width(130.0),
        );
        if ui
            .add_enabled(has_text, egui::Button::new("💾 Save as prompt"))
            .on_hover_text("Keep these instructions as a chip you can pick for any project")
            .clicked()
        {
            actions.push(Action::SavePrompt(info.path.clone()));
        }
    });
    ui.checkbox(&mut form.claude_may_run_git, "Let Claude run git and gh").on_hover_text(
        "Off: Claude can only read and edit files.\nOn: it can also run git and gh commands, e.g. to commit, push or change repository settings itself.",
    );

    let count = chosen_tasks(form, saved).len() + usize::from(!form.claude_prompt.trim().is_empty());
    ui.add_space(6.0);
    ui.horizontal_wrapped(|ui| {
        let label = match count {
            0 => "Pick a task".to_string(),
            1 => "▶ Run 1 task".to_string(),
            n => format!("▶ Run {n} tasks"),
        };
        let run = egui::Button::new(RichText::new(label).strong());
        if (ui.add_enabled(count > 0 && !view.busy, run).on_hover_text("Ctrl+Enter").clicked() || ctrl_enter)
            && count > 0
            && !view.busy
        {
            actions.push(Action::RunTasks {
                path: info.path.clone(),
                on_checked: false,
            });
        }
        if view.checked > 0 {
            let others = egui::Button::new(format!("▶ Run on {} checked project(s)", view.checked));
            if ui
                .add_enabled(count > 0, others)
                .on_hover_text("Runs the same tasks on every ticked project, one after another")
                .clicked()
            {
                actions.push(Action::RunTasks {
                    path: info.path.clone(),
                    on_checked: true,
                });
            }
        }
    });
    for (id, label) in view.running.iter().filter(|(_, l)| l.starts_with("Claude")) {
        ui.horizontal(|ui| {
            ui.spinner();
            ui.label(RichText::new(label).color(CLAUDE));
            if ui.small_button("Cancel").clicked() {
                actions.push(Action::Cancel(*id));
            }
        });
    }
}

fn style(ctx: &egui::Context) {
    use egui::{FontId, TextStyle};
    ctx.all_styles_mut(|style| {
        style.text_styles.insert(TextStyle::Body, FontId::proportional(14.5));
        style.text_styles.insert(TextStyle::Button, FontId::proportional(14.5));
        style.text_styles.insert(TextStyle::Small, FontId::proportional(12.0));
        style.text_styles.insert(TextStyle::Monospace, FontId::monospace(13.0));
        style.text_styles.insert(TextStyle::Heading, FontId::proportional(20.0));
        style.spacing.item_spacing = vec2(8.0, 6.0);
        style.spacing.button_padding = vec2(10.0, 4.0);
        style.spacing.interact_size.y = 24.0;
    });
}

/// The window icon: an upload arrow on a purple tile.
pub fn icon() -> egui::IconData {
    const SIZE: usize = 64;
    let mut rgba = vec![0u8; SIZE * SIZE * 4];
    let size = SIZE as f32;
    let radius = 14.0;
    for y in 0..SIZE {
        for x in 0..SIZE {
            let (fx, fy) = (x as f32 + 0.5, y as f32 + 0.5);
            let (cx, cy) = (fx.clamp(radius, size - radius), fy.clamp(radius, size - radius));
            if (fx - cx).powi(2) + (fy - cy).powi(2) > radius * radius {
                continue;
            }
            let shaft = (27.0..=37.0).contains(&fx) && (28.0..=50.0).contains(&fy);
            let head = (12.0..=30.0).contains(&fy) && (fx - 32.0).abs() <= (fy - 12.0) * (17.0 / 18.0);
            let color = if shaft || head { [255, 255, 255, 255] } else { [0x6e, 0x40, 0xc9, 255] };
            rgba[(y * SIZE + x) * 4..][..4].copy_from_slice(&color);
        }
    }
    egui::IconData {
        rgba,
        width: SIZE as u32,
        height: SIZE as u32,
    }
}
