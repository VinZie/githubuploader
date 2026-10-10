# GitHub Uploader

A Windows desktop app (Rust + egui) for publishing the projects in `Desktop\claude-stuff` to GitHub, with Claude Code as a helper.

## What it does

- Groups every project folder by status: not on GitHub yet, on GitHub, or empty.
- **Prepare & upload** in one click. Claude tidies the repo (`.gitignore`, a missing README, description and topics). Then the app checks the files, commits, creates the repository, pushes, and sends invitations. The progress shows step by step, and a result card links to the repo.
- A checklist on each project shows what's ready and what will happen before you upload.
- Creates repositories as **private** or **public** under your account or an organization, with a description, website, topics, license and Issues/Wiki/Projects/Discussions switches.
- Manages **collaborators**: invite people, change their permission level, withdraw invitations, remove access.
- For repos that are already on GitHub: commits and pushes changes, and saves changed settings.
- Runs a **pre-flight check** before anything is uploaded. It flags files over GitHub's 100 MB limit, secret files (`.env`, keys), tokens inside files, and dependency or build folders that aren't ignored. **Ignore** adds a flagged item to `.gitignore` in one click.
- Uploads several ticked projects in one go, after a review screen where you set each one's visibility.
- **Claude tasks**: pick any number of them, such as README, `.gitignore`, secrets review, `.env.example`, CI workflow, code review or explanation. You can add your own saved prompts and extra instructions, then run them all as one request on one project or on every ticked project.
- **Settings** set the Claude model and the defaults for new repositories: visibility, license, people to always invite, and whether Claude prepares repos first.

## Requirements

- [Git](https://git-scm.com/download/win)
- [GitHub CLI](https://cli.github.com), signed in with `gh auth login`
- [Claude Code](https://claude.com/claude-code), for the ✨ features (optional)

## Building

```
cargo build --release
```

The executable is `target\release\githubuploader.exe`; the copy the desktop shortcut runs is `dist\GitHub Uploader.exe`. With the GNU Rust toolchain, MinGW's `dlltool` and `windres` (which embeds the icon from `assets\icon.ico`) must be on `PATH`. WinLibs provides both.

Settings live in `%APPDATA%\githubuploader\config.json`.
