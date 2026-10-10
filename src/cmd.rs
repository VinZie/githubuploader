//! Running git, gh and claude without flashing console windows, with their
//! output optionally streamed into the activity log.

use std::ffi::OsString;
use std::io::{BufRead, BufReader, Write};
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use crate::jobs::{Job, LogKind};

/// CREATE_NO_WINDOW: console programs started from a GUI app would
/// otherwise each pop up their own console.
pub const NO_WINDOW: u32 = 0x0800_0000;

pub struct Out {
    pub ok: bool,
    pub stdout: String,
    pub stderr: String,
}

impl Out {
    pub fn text(&self) -> &str {
        self.stdout.trim()
    }

    /// The most useful explanation of a failure.
    pub fn err_text(&self) -> String {
        let err = self.stderr.trim();
        if err.is_empty() {
            self.stdout.trim().to_string()
        } else {
            err.to_string()
        }
    }
}

pub struct Cmd {
    program: PathBuf,
    args: Vec<OsString>,
    cwd: Option<PathBuf>,
    stdin: Option<String>,
    echo: bool,
    stream: bool,
}

pub fn git(dir: &Path) -> Cmd {
    Cmd::new("git").cwd(dir)
}

pub fn gh() -> Cmd {
    Cmd::new("gh")
}

impl Cmd {
    pub fn new(program: impl Into<PathBuf>) -> Self {
        Self {
            program: program.into(),
            args: Vec::new(),
            cwd: None,
            stdin: None,
            echo: false,
            stream: false,
        }
    }

    pub fn arg(mut self, arg: impl Into<OsString>) -> Self {
        self.args.push(arg.into());
        self
    }

    pub fn args<S: Into<OsString>>(mut self, args: impl IntoIterator<Item = S>) -> Self {
        self.args.extend(args.into_iter().map(Into::into));
        self
    }

    pub fn cwd(mut self, dir: &Path) -> Self {
        self.cwd = Some(dir.to_path_buf());
        self
    }

    pub fn stdin(mut self, input: impl Into<String>) -> Self {
        self.stdin = Some(input.into());
        self
    }

    /// Log the command line before running it.
    pub fn echo(mut self) -> Self {
        self.echo = true;
        self
    }

    /// Log the command's output as it arrives.
    pub fn stream(mut self) -> Self {
        self.stream = true;
        self
    }

    fn display(&self) -> String {
        let program = self
            .program
            .file_stem()
            .map_or_else(|| self.program.to_string_lossy(), |s| s.to_string_lossy());
        let mut line = program.into_owned();
        for arg in &self.args {
            let arg = arg.to_string_lossy();
            let arg = if arg.contains('\n') {
                "<message>".into()
            } else {
                arg
            };
            if arg.is_empty() || arg.contains(' ') {
                line.push_str(&format!(" \"{arg}\""));
            } else {
                line.push(' ');
                line.push_str(&arg);
            }
        }
        line
    }

    pub fn run(&self, job: Option<&Job>) -> Result<Out, String> {
        self.run_lines(job, |_| {})
    }

    /// Runs the command and returns its stdout, or an error if it failed.
    pub fn run_ok(&self, job: Option<&Job>) -> Result<String, String> {
        let out = self.run(job)?;
        if out.ok {
            Ok(out.stdout)
        } else {
            Err(format!("`{}` failed: {}", self.display(), out.err_text()))
        }
    }

    /// Runs the command, handing each stdout line to `on_line` as it arrives.
    pub fn run_lines(&self, job: Option<&Job>, mut on_line: impl FnMut(&str)) -> Result<Out, String> {
        if let Some(job) = job {
            job.check()?;
            if self.echo {
                job.log(LogKind::Cmd, format!("$ {}", self.display()));
            }
        }
        let mut command = Command::new(&self.program);
        command
            .args(&self.args)
            .creation_flags(NO_WINDOW)
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("GH_PROMPT_DISABLED", "1")
            .env("GH_NO_UPDATE_NOTIFIER", "1")
            .env("NO_COLOR", "1")
            .stdin(if self.stdin.is_some() {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if let Some(dir) = &self.cwd {
            command.current_dir(dir);
        }
        let mut child = command
            .spawn()
            .map_err(|err| format!("couldn't start {}: {err}", self.program.display()))?;
        if let Some(job) = job {
            job.cancel.set_pid(Some(child.id()));
        }
        if let (Some(input), Some(mut pipe)) = (self.stdin.clone(), child.stdin.take()) {
            // Dropping the pipe afterwards closes stdin.
            std::thread::spawn(move || {
                let _ = pipe.write_all(input.as_bytes());
            });
        }

        let stderr = child.stderr.take().expect("stderr is piped");
        let stderr_job = job.filter(|_| self.stream).cloned();
        let stderr_reader = std::thread::spawn(move || {
            let mut all = String::new();
            for line in BufReader::new(stderr).split(b'\n') {
                let Ok(line) = line else { break };
                let line = String::from_utf8_lossy(&line);
                let line = line.trim_end_matches('\r');
                if let Some(job) = &stderr_job {
                    job.log(LogKind::Out, line);
                }
                all.push_str(line);
                all.push('\n');
            }
            all
        });

        let mut stdout = String::new();
        let pipe = child.stdout.take().expect("stdout is piped");
        for line in BufReader::new(pipe).split(b'\n') {
            let Ok(line) = line else { break };
            let line = String::from_utf8_lossy(&line);
            let line = line.trim_end_matches('\r');
            if self.stream {
                if let Some(job) = job {
                    job.log(LogKind::Out, line);
                }
            }
            on_line(line);
            stdout.push_str(line);
            stdout.push('\n');
        }

        let status = child.wait().map_err(|err| err.to_string());
        if let Some(job) = job {
            job.cancel.set_pid(None);
        }
        let stderr = stderr_reader.join().unwrap_or_default();
        if let Some(job) = job {
            job.check()?;
        }
        Ok(Out {
            ok: status?.success(),
            stdout,
            stderr,
        })
    }
}
