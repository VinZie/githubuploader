//! Background work: every slow operation runs on its own thread as a [`Job`]
//! and reports back to the UI through [`Msg`]s on a [`Bus`].

use std::os::windows::process::CommandExt;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};

use eframe::egui;

use crate::cmd::NO_WINDOW;
use crate::github::{Account, RemoteSettings};
use crate::project::{ProjectInfo, ScanReport};

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum LogKind {
    Info,
    Cmd,
    Out,
    Ok,
    Warn,
    Err,
    Claude,
    Tool,
}

pub struct LogLine {
    pub kind: LogKind,
    pub tag: String,
    pub text: String,
}

/// Versions of the command-line tools the app drives; `None` when missing.
#[derive(Clone, Default)]
pub struct Tools {
    pub git: Option<String>,
    pub gh: Option<String>,
    pub claude: Option<String>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum StepState {
    Pending,
    Running,
    Done,
    /// Stopped for the user to look at something.
    Attention,
    Failed,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum OutcomeKind {
    Success,
    Attention,
    Failed,
}

/// How an upload ended, shown on the project until the next one.
#[derive(Clone)]
pub struct Outcome {
    pub kind: OutcomeKind,
    pub text: String,
    pub url: Option<String>,
}

pub enum Msg {
    Log(LogLine),
    Tools(Tools),
    Account(Result<Account, String>),
    /// Global git `user.name` and `user.email`, if both are set.
    Identity(Option<(String, String)>),
    Projects(Vec<ProjectInfo>),
    Project(ProjectInfo),
    Remote(PathBuf, Result<RemoteSettings, String>),
    Scan(PathBuf, ScanReport),
    Suggestion {
        path: PathBuf,
        description: Option<String>,
        topics: Vec<String>,
        /// Replace what the user typed, rather than only filling blanks.
        overwrite: bool,
    },
    CommitMsg(PathBuf, String),
    /// The steps an upload is about to go through.
    Plan(PathBuf, Vec<String>),
    Step {
        path: PathBuf,
        index: usize,
        state: StepState,
        note: Option<String>,
    },
    Outcome(PathBuf, Outcome),
    /// A short pop-up message.
    Toast(LogKind, String),
    JobDone(u64),
}

#[derive(Clone)]
pub struct Bus {
    tx: Sender<Msg>,
    ctx: egui::Context,
}

impl Bus {
    pub fn new(tx: Sender<Msg>, ctx: egui::Context) -> Self {
        Self { tx, ctx }
    }

    pub fn send(&self, msg: Msg) {
        let _ = self.tx.send(msg);
        self.ctx.request_repaint();
    }

    pub fn ctx(&self) -> &egui::Context {
        &self.ctx
    }

    pub fn log(&self, kind: LogKind, tag: &str, text: impl AsRef<str>) {
        for line in text.as_ref().lines().filter(|l| !l.trim().is_empty()) {
            self.send(Msg::Log(LogLine {
                kind,
                tag: tag.to_string(),
                text: line.to_string(),
            }));
        }
    }
}

/// Lets the UI stop a job: sets a flag the job checks between steps and
/// kills whatever process it is currently waiting on.
#[derive(Clone, Default)]
pub struct Cancel {
    flag: Arc<AtomicBool>,
    pid: Arc<Mutex<Option<u32>>>,
}

impl Cancel {
    pub fn cancel(&self) {
        self.flag.store(true, Ordering::SeqCst);
        if let Some(pid) = *self.pid.lock().unwrap() {
            // /T takes down the whole tree, e.g. the shells Claude spawned.
            let _ = Command::new("taskkill")
                .args(["/T", "/F", "/PID", &pid.to_string()])
                .creation_flags(NO_WINDOW)
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
        }
    }

    pub fn is_cancelled(&self) -> bool {
        self.flag.load(Ordering::SeqCst)
    }

    pub fn set_pid(&self, pid: Option<u32>) {
        *self.pid.lock().unwrap() = pid;
    }
}

#[derive(Clone)]
pub struct Job {
    pub id: u64,
    /// Shown next to every log line, usually the project name.
    pub tag: String,
    pub bus: Bus,
    pub cancel: Cancel,
}

impl Job {
    pub fn log(&self, kind: LogKind, text: impl AsRef<str>) {
        self.bus.log(kind, &self.tag, text);
    }

    pub fn info(&self, text: impl AsRef<str>) {
        self.log(LogKind::Info, text);
    }

    pub fn ok(&self, text: impl AsRef<str>) {
        self.log(LogKind::Ok, text);
    }

    pub fn warn(&self, text: impl AsRef<str>) {
        self.log(LogKind::Warn, text);
    }

    pub fn send(&self, msg: Msg) {
        self.bus.send(msg);
    }

    /// Bails out of the job once the user has pressed Cancel.
    pub fn check(&self) -> Result<(), String> {
        if self.cancel.is_cancelled() {
            Err("Cancelled.".into())
        } else {
            Ok(())
        }
    }

    /// The same job, logging under a different tag (batch uploads).
    pub fn tagged(&self, tag: &str) -> Job {
        Job {
            tag: tag.to_string(),
            ..self.clone()
        }
    }
}
