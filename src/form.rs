//! What the user has chosen for one project's repository.

use std::collections::BTreeSet;

use crate::config::Config;
use crate::github::{Collab, CollabKind, Perm, RemoteSettings};
use crate::project::{self, ProjectInfo};

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum License {
    None,
    Mit,
    Apache,
    Gpl3,
    Bsd3,
    Mpl2,
    Unlicense,
}

impl License {
    pub const ALL: [License; 7] = [
        License::None,
        License::Mit,
        License::Apache,
        License::Gpl3,
        License::Bsd3,
        License::Mpl2,
        License::Unlicense,
    ];

    pub fn label(self) -> &'static str {
        match self {
            License::None => "No license",
            License::Mit => "MIT",
            License::Apache => "Apache 2.0",
            License::Gpl3 => "GPL v3",
            License::Bsd3 => "BSD 3-Clause",
            License::Mpl2 => "MPL 2.0",
            License::Unlicense => "Unlicense (public domain)",
        }
    }

    pub fn from_key(key: &str) -> License {
        License::ALL.into_iter().find(|l| l.key() == Some(key)).unwrap_or(License::None)
    }

    /// Key in GitHub's license catalog.
    pub fn key(self) -> Option<&'static str> {
        match self {
            License::None => None,
            License::Mit => Some("mit"),
            License::Apache => Some("apache-2.0"),
            License::Gpl3 => Some("gpl-3.0"),
            License::Bsd3 => Some("bsd-3-clause"),
            License::Mpl2 => Some("mpl-2.0"),
            License::Unlicense => Some("unlicense"),
        }
    }
}

#[derive(Clone)]
pub struct RepoForm {
    pub owner: String,
    pub name: String,
    pub description: String,
    pub homepage: String,
    /// Comma-separated, cleaned up by [`project::topics`].
    pub topics: String,
    pub private: bool,
    pub issues: bool,
    pub wiki: bool,
    pub projects: bool,
    pub discussions: bool,
    pub license: License,
    pub collaborators: Vec<Collab>,
    pub new_collaborator: String,
    pub new_perm: Perm,
    pub commit_message: String,
    /// The user looked at the pre-flight warnings and wants to go ahead.
    pub accept_warnings: bool,
    /// Let Claude tidy the project up as the first step of uploading it.
    pub claude_prepare: bool,
    /// Chosen Claude tasks: built-in ids and [`tasks::saved_id`]s.
    pub claude_tasks: BTreeSet<String>,
    /// Extra instructions for Claude, sent along with the chosen tasks.
    pub claude_prompt: String,
    /// Name for saving `claude_prompt` as a reusable prompt.
    pub prompt_name: String,
    pub claude_may_run_git: bool,
    /// The settings on GitHub, once loaded, for repos that are already there.
    pub remote: Option<RemoteSettings>,
    pub remote_error: Option<String>,
}

impl RepoForm {
    pub fn new(info: &ProjectInfo, owner: &str, defaults: &Config) -> Self {
        let (owner, name) = match &info.github {
            Some((owner, name)) => (owner.clone(), name.clone()),
            None => (owner.to_string(), project::repo_name(&info.name)),
        };
        let first_push = info.github.is_none() && !info.has_commits;
        let mut form = Self {
            owner,
            name,
            description: String::new(),
            homepage: String::new(),
            topics: String::new(),
            private: true,
            issues: true,
            wiki: true,
            projects: true,
            discussions: false,
            license: License::None,
            collaborators: Vec::new(),
            new_collaborator: String::new(),
            new_perm: Perm::Write,
            commit_message: if first_push { "Initial commit".into() } else { String::new() },
            accept_warnings: false,
            claude_prepare: true,
            claude_tasks: BTreeSet::new(),
            claude_prompt: String::new(),
            prompt_name: String::new(),
            claude_may_run_git: false,
            remote: None,
            remote_error: None,
        };
        if info.github.is_none() {
            form.apply_defaults(defaults);
        }
        form
    }

    /// Takes on the app-wide defaults for new repositories.
    pub fn apply_defaults(&mut self, defaults: &Config) {
        self.private = defaults.default_private;
        self.license = License::from_key(&defaults.default_license);
        self.claude_prepare = defaults.claude_prepares;
        for login in &defaults.default_collaborators {
            if !self.collaborators.iter().any(|c| c.login.eq_ignore_ascii_case(login)) {
                self.collaborators.push(Collab::new(login, Perm::Write));
            }
        }
    }

    pub fn slug(&self) -> String {
        format!("{}/{}", self.owner, project::repo_name(&self.name))
    }

    pub fn topic_list(&self) -> Vec<String> {
        project::topics(&self.topics)
    }

    /// Takes on the settings GitHub has for the repository.
    pub fn load_remote(&mut self, remote: RemoteSettings) {
        self.description = remote.description.clone();
        self.homepage = remote.homepage.clone();
        self.topics = remote.topics.join(", ");
        self.private = remote.private;
        self.issues = remote.issues;
        self.wiki = remote.wiki;
        self.projects = remote.projects;
        self.discussions = remote.discussions;
        self.collaborators = remote.collaborators.clone();
        self.remote_error = None;
        self.remote = Some(remote);
    }

    /// Copies the choices that make sense across projects (not name or description).
    pub fn copy_shared_settings(&mut self, from: &RepoForm) {
        self.private = from.private;
        self.issues = from.issues;
        self.wiki = from.wiki;
        self.projects = from.projects;
        self.discussions = from.discussions;
        self.license = from.license;
        self.claude_prepare = from.claude_prepare;
        for collab in from.collaborators.iter().filter(|c| !c.remove) {
            if !self.collaborators.iter().any(|c| c.login.eq_ignore_ascii_case(&collab.login)) {
                self.collaborators.push(Collab::new(&collab.login, collab.perm));
            }
        }
    }

    /// Whether anything differs from what GitHub has.
    pub fn has_unapplied_changes(&self) -> bool {
        let Some(remote) = &self.remote else { return false };
        self.description != remote.description
            || self.homepage != remote.homepage
            || self.private != remote.private
            || self.issues != remote.issues
            || self.wiki != remote.wiki
            || self.projects != remote.projects
            || self.discussions != remote.discussions
            || self.topic_list() != remote.topics
            || self
                .collaborators
                .iter()
                .any(|c| c.remove || c.kind == CollabKind::New || c.perm != c.saved_perm)
    }
}
