//! GitHub access through the `gh` CLI, which already holds the user's login.

use serde_json::Value;

use crate::cmd::gh;
use crate::jobs::Job;

#[derive(Clone)]
pub struct Account {
    pub login: String,
    pub id: u64,
    pub name: Option<String>,
    pub orgs: Vec<String>,
}

impl Account {
    /// Name for commits when git has no identity configured.
    pub fn display_name(&self) -> &str {
        self.name.as_deref().filter(|n| !n.is_empty()).unwrap_or(&self.login)
    }

    /// GitHub's private commit address, so commits never expose a real email.
    pub fn noreply_email(&self) -> String {
        format!("{}+{}@users.noreply.github.com", self.id, self.login)
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Perm {
    Read,
    Triage,
    Write,
    Maintain,
    Admin,
}

impl Perm {
    pub const ALL: [Perm; 5] = [Perm::Read, Perm::Triage, Perm::Write, Perm::Maintain, Perm::Admin];

    pub fn label(self) -> &'static str {
        match self {
            Perm::Read => "Read",
            Perm::Triage => "Triage",
            Perm::Write => "Write",
            Perm::Maintain => "Maintain",
            Perm::Admin => "Admin",
        }
    }

    /// Value for the collaborators API.
    fn collaborator_value(self) -> &'static str {
        match self {
            Perm::Read => "pull",
            Perm::Triage => "triage",
            Perm::Write => "push",
            Perm::Maintain => "maintain",
            Perm::Admin => "admin",
        }
    }

    /// Value for the invitations API, which names the levels differently.
    fn invitation_value(self) -> &'static str {
        match self {
            Perm::Read => "read",
            Perm::Triage => "triage",
            Perm::Write => "write",
            Perm::Maintain => "maintain",
            Perm::Admin => "admin",
        }
    }

    fn parse(value: &str) -> Perm {
        match value {
            "read" | "pull" => Perm::Read,
            "triage" => Perm::Triage,
            "maintain" => Perm::Maintain,
            "admin" => Perm::Admin,
            _ => Perm::Write,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum CollabKind {
    /// Added in the app, not sent to GitHub yet.
    New,
    Member,
    Invited(u64),
}

#[derive(Clone, Debug)]
pub struct Collab {
    pub login: String,
    pub perm: Perm,
    /// The permission GitHub has, to spot edits.
    pub saved_perm: Perm,
    pub kind: CollabKind,
    pub remove: bool,
}

impl Collab {
    pub fn new(login: &str, perm: Perm) -> Self {
        Self {
            login: login.to_string(),
            perm,
            saved_perm: perm,
            kind: CollabKind::New,
            remove: false,
        }
    }
}

/// A repository's settings as GitHub has them.
#[derive(Clone, Default)]
pub struct RemoteSettings {
    pub description: String,
    pub homepage: String,
    pub private: bool,
    pub topics: Vec<String>,
    pub issues: bool,
    pub wiki: bool,
    pub projects: bool,
    pub discussions: bool,
    pub collaborators: Vec<Collab>,
}

fn json(text: &str) -> Result<Value, String> {
    serde_json::from_str(text).map_err(|err| format!("unexpected reply from gh: {err}"))
}

fn text(value: &Value, key: &str) -> String {
    value[key].as_str().unwrap_or_default().to_string()
}

pub fn account(job: Option<&Job>) -> Result<Account, String> {
    let user = json(&gh().args(["api", "user"]).run_ok(job)?)?;
    let orgs = gh()
        .args(["api", "user/orgs"])
        .run_ok(job)
        .ok()
        .and_then(|text| json(&text).ok())
        .and_then(|orgs| {
            orgs.as_array()
                .map(|orgs| orgs.iter().map(|org| text(org, "login")).collect())
        })
        .unwrap_or_default();
    Ok(Account {
        login: text(&user, "login"),
        id: user["id"].as_u64().unwrap_or_default(),
        name: user["name"].as_str().map(str::to_string),
        orgs,
    })
}

pub fn exists(job: &Job, slug: &str) -> Result<bool, String> {
    Ok(gh().args(["repo", "view", slug, "--json", "name"]).run(Some(job))?.ok)
}

pub fn load(job: &Job, slug: &str) -> Result<RemoteSettings, String> {
    let repo = json(
        &gh()
            .args(["repo", "view", slug, "--json"])
            .arg(
                "description,homepageUrl,isPrivate,repositoryTopics,hasIssuesEnabled,\
                 hasWikiEnabled,hasProjectsEnabled,hasDiscussionsEnabled,owner",
            )
            .run_ok(Some(job))?,
    )?;
    let owner = text(&repo["owner"], "login");
    let topics = repo["repositoryTopics"]
        .as_array()
        .map(|topics| topics.iter().map(|t| text(t, "name")).collect())
        .unwrap_or_default();

    let mut collaborators = Vec::new();
    // Listing collaborators needs admin rights; without them, show none.
    if let Ok(list) = gh()
        .args(["api", &format!("repos/{slug}/collaborators?affiliation=direct&per_page=100")])
        .run_ok(Some(job))
    {
        for person in json(&list)?.as_array().into_iter().flatten() {
            let login = text(person, "login");
            if login.eq_ignore_ascii_case(&owner) {
                continue;
            }
            let perm = Perm::parse(person["role_name"].as_str().unwrap_or("write"));
            collaborators.push(Collab {
                kind: CollabKind::Member,
                ..Collab::new(&login, perm)
            });
        }
    }
    if let Ok(list) = gh()
        .args(["api", &format!("repos/{slug}/invitations?per_page=100")])
        .run_ok(Some(job))
    {
        for invite in json(&list)?.as_array().into_iter().flatten() {
            let perm = Perm::parse(invite["permissions"].as_str().unwrap_or("write"));
            collaborators.push(Collab {
                kind: CollabKind::Invited(invite["id"].as_u64().unwrap_or_default()),
                ..Collab::new(&text(&invite["invitee"], "login"), perm)
            });
        }
    }

    Ok(RemoteSettings {
        description: text(&repo, "description"),
        homepage: text(&repo, "homepageUrl"),
        private: repo["isPrivate"].as_bool().unwrap_or(true),
        topics,
        issues: repo["hasIssuesEnabled"].as_bool().unwrap_or(true),
        wiki: repo["hasWikiEnabled"].as_bool().unwrap_or(true),
        projects: repo["hasProjectsEnabled"].as_bool().unwrap_or(true),
        discussions: repo["hasDiscussionsEnabled"].as_bool().unwrap_or(false),
        collaborators,
    })
}

/// Invites someone, or changes the permission of an existing collaborator.
pub fn put_collaborator(job: &Job, slug: &str, login: &str, perm: Perm) -> Result<(), String> {
    gh().args(["api", "-X", "PUT", &format!("repos/{slug}/collaborators/{login}")])
        .args(["-f", &format!("permission={}", perm.collaborator_value())])
        .echo()
        .run_ok(Some(job))
        .map(drop)
}

pub fn remove_collaborator(job: &Job, slug: &str, login: &str) -> Result<(), String> {
    gh().args(["api", "-X", "DELETE", &format!("repos/{slug}/collaborators/{login}")])
        .echo()
        .run_ok(Some(job))
        .map(drop)
}

pub fn update_invitation(job: &Job, slug: &str, id: u64, perm: Perm) -> Result<(), String> {
    gh().args(["api", "-X", "PATCH", &format!("repos/{slug}/invitations/{id}")])
        .args(["-f", &format!("permissions={}", perm.invitation_value())])
        .echo()
        .run_ok(Some(job))
        .map(drop)
}

pub fn cancel_invitation(job: &Job, slug: &str, id: u64) -> Result<(), String> {
    gh().args(["api", "-X", "DELETE", &format!("repos/{slug}/invitations/{id}")])
        .echo()
        .run_ok(Some(job))
        .map(drop)
}

/// A license's text from GitHub's license catalog.
pub fn license_text(job: &Job, key: &str) -> Result<String, String> {
    let license = json(&gh().args(["api", &format!("licenses/{key}")]).run_ok(Some(job))?)?;
    Ok(text(&license, "body"))
}
