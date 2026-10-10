//! Ready-made requests for Claude. Any number of them, plus the user's own
//! saved prompts and extra instructions, go to Claude as one run.

pub struct Task {
    pub id: &'static str,
    pub label: &'static str,
    pub prompt: &'static str,
}

pub const BUILT_IN: &[Task] = &[
    Task {
        id: "readme",
        label: "Write README",
        prompt: "Write or improve README.md: a short summary of what the project does, its main features, and how \
                 to build and run it. Only describe what the code actually does.",
    },
    Task {
        id: "gitignore",
        label: "Improve .gitignore",
        prompt: "Update .gitignore so dependency folders, build output, caches, virtual environments, large \
                 binaries and secret files stay out of the repository. Only add rules; never remove existing ones.",
    },
    Task {
        id: "secrets",
        label: "Secrets review",
        prompt: "Review the project for anything that shouldn't be published: API keys, tokens, passwords, \
                 personal data, hard-coded private paths and large generated files. Report each finding with its \
                 file path and how to fix it. Don't change files for this task.",
    },
    Task {
        id: "env-example",
        label: "Add .env.example",
        prompt: "If the project reads settings from environment variables or a .env file, add a .env.example that \
                 lists every variable with a placeholder value and a short comment, never a real value. Skip this \
                 if the project doesn't use any.",
    },
    Task {
        id: "ci",
        label: "Add CI workflow",
        prompt: "Add a minimal GitHub Actions workflow at .github/workflows/ci.yml that builds and tests the \
                 project on every push and pull request.",
    },
    Task {
        id: "review",
        label: "Code review",
        prompt: "Look over the code for bugs and rough edges worth fixing before it's published. Report them with \
                 file paths, most important first. Don't change files for this task.",
    },
    Task {
        id: "explain",
        label: "Explain project",
        prompt: "Briefly explain what the project does and how it's organised. Don't change files for this task.",
    },
];

/// Id of a saved prompt in a selection, kept apart from the built-in ids.
pub fn saved_id(name: &str) -> String {
    format!("saved:{name}")
}

/// One request covering every chosen task (label and prompt) plus any extra
/// instructions. `None` when there's nothing to do.
pub fn combine(tasks: &[(String, String)], extra: &str) -> Option<String> {
    let extra = extra.trim();
    match (tasks, extra.is_empty()) {
        ([], true) => None,
        ([], false) => Some(extra.to_string()),
        ([(_, prompt)], true) => Some(prompt.clone()),
        _ => {
            let mut request = String::from("Do each of these tasks, in order:\n");
            for (i, (label, prompt)) in tasks.iter().enumerate() {
                request += &format!("\n{}. {label}: {prompt}\n", i + 1);
            }
            if !extra.is_empty() {
                request += &format!("\n{}. Also: {extra}\n", tasks.len() + 1);
            }
            request += "\nFinish with a short summary of what you did for each task.";
            Some(request)
        }
    }
}

/// A short name for a prompt the user saves without naming it.
pub fn default_name(text: &str) -> String {
    let words: Vec<&str> = text.split_whitespace().take(4).collect();
    let mut name = words.join(" ");
    if text.split_whitespace().count() > 4 {
        name.push('…');
    }
    name
}

#[cfg(test)]
mod tests {
    use super::*;

    fn task(label: &str, prompt: &str) -> (String, String) {
        (label.to_string(), prompt.to_string())
    }

    #[test]
    fn nothing_selected() {
        assert_eq!(combine(&[], "  "), None);
    }

    #[test]
    fn single_task_is_sent_as_is() {
        assert_eq!(combine(&[task("A", "Do A.")], ""), Some("Do A.".into()));
        assert_eq!(combine(&[], "Just this"), Some("Just this".into()));
    }

    #[test]
    fn several_tasks_are_numbered() {
        let request = combine(&[task("A", "Do A."), task("B", "Do B.")], "And C").unwrap();
        assert!(request.contains("1. A: Do A."));
        assert!(request.contains("2. B: Do B."));
        assert!(request.contains("3. Also: And C"));
    }

    #[test]
    fn names_from_text() {
        assert_eq!(default_name("Add a screenshots section please"), "Add a screenshots section…");
        assert_eq!(default_name("Fix typos"), "Fix typos");
    }
}
