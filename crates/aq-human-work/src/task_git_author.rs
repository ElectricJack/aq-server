//! Trusted, task-scoped Git attribution from confirmed creation journals.
use serde::{Deserialize, Serialize};
use sqlx::{PgExecutor, PgPool};
use uuid::Uuid;

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub struct TaskGitAuthor {
    pub name: String,
    pub email: String,
}

/// Look up the approved publication identity for a server-resolved task.
/// Missing provenance or an identity Git cannot preserve returns `None`.
pub async fn lookup<'e>(
    executor: impl PgExecutor<'e>,
    project_id: Uuid,
    workspace_id: Uuid,
    task_id: Uuid,
) -> Result<Option<TaskGitAuthor>, sqlx::Error> {
    let row: Option<(String, String)> = sqlx::query_as(
        "SELECT name, email FROM task_git_authors
         WHERE project_id = $1 AND workspace_id = $2 AND task_id = $3 AND confirmed",
    )
    .bind(project_id)
    .bind(workspace_id)
    .bind(task_id)
    .fetch_optional(executor)
    .await?;
    Ok(row
        .map(|(name, email)| TaskGitAuthor { name, email })
        .filter(TaskGitAuthor::valid_for_git))
}

impl TaskGitAuthor {
    fn valid_for_git(&self) -> bool {
        valid_name(&self.name) && valid_email(&self.email)
    }
}

fn signature_char(c: char) -> bool {
    !c.is_control() && !matches!(c, '<' | '>')
}
fn preserves_edges(s: &str) -> bool {
    // Git strips these characters at both ends of a signature field.
    let stripped = |c: char| {
        c.is_ascii_whitespace() || matches!(c, ',' | ':' | ';' | '<' | '>' | '"' | '\'' | '\\')
    };
    !s.starts_with(stripped) && !s.ends_with(stripped)
}
fn valid_name(name: &str) -> bool {
    !name.trim().is_empty()
        && name.chars().count() <= 120
        && name.chars().all(signature_char)
        && preserves_edges(name)
}
fn valid_email(email: &str) -> bool {
    let address = email.split_once('@').is_some_and(|(local, domain)| {
        !local.is_empty() && !domain.is_empty() && !domain.contains('@')
    });
    address
        && email.len() <= 254
        && email
            .chars()
            .all(|c| signature_char(c) && !c.is_whitespace())
        && preserves_edges(email)
}

pub(crate) async fn for_tasks(
    pool: &PgPool,
    project_id: Uuid,
    workspace_id: Uuid,
    tasks: &[Uuid],
) -> Result<Vec<(Uuid, String, String, Option<Uuid>)>, sqlx::Error> {
    sqlx::query_as(
        "SELECT task_id, name, email, inherited_from FROM task_git_authors
         WHERE project_id = $1 AND workspace_id = $2 AND task_id = ANY($3) AND confirmed",
    )
    .bind(project_id)
    .bind(workspace_id)
    .bind(tasks)
    .fetch_all(pool)
    .await
}

#[cfg(test)]
mod tests {
    use super::TaskGitAuthor;

    #[test]
    fn git_identity_accepts_plus_tags_and_rejects_signature_injection() {
        let author = |name: &str, email: &str| TaskGitAuthor {
            name: name.into(),
            email: email.into(),
        };
        assert!(author("First.Last+tag", "First.Last+tag@Example.Test").valid_for_git());
        assert!(author("Zoë", "Zoë@localhost").valid_for_git());
        for email in [
            "",
            "@example.test",
            "local@",
            "double@@example.test",
            "line\nbreak@example.test",
            "a\u{2003}b@example.test",
            "a<b@example.test",
            "a>b@example.test",
            "a\u{0007}b@example.test",
            &format!("{}@example.test", "x".repeat(242)),
        ] {
            assert!(!author("local", email).valid_for_git(), "{email:?}");
        }
        for name in ["", "  ", "a<b", "a>b", "a\rb", &"界".repeat(121)] {
            assert!(
                !author(name, "local@example.test").valid_for_git(),
                "{name:?}"
            );
        }
        assert!(author(&"界".repeat(120), "local@example.test").valid_for_git());
    }
    #[test]
    fn git_formatter_preserves_accepted_identity_and_changes_rejected_boundaries() {
        let render = |name: &str, email: &str| {
            let output = std::process::Command::new("git")
                .args(["var", "GIT_AUTHOR_IDENT"])
                .env("GIT_AUTHOR_NAME", name)
                .env("GIT_AUTHOR_EMAIL", email)
                .env("GIT_AUTHOR_DATE", "1700000000 +0000")
                .output()
                .unwrap();
            assert!(output.status.success());
            String::from_utf8(output.stdout).unwrap()
        };
        let (name, email) = ("First.Last+Tag", "First.Last+Tag@Example.Test");
        assert_eq!(
            render(name, email),
            format!("{name} <{email}> 1700000000 +0000\n")
        );
        for (name, email) in [
            ("'First", "first@example.test"),
            ("First:", "first@example.test"),
            ("First", "'first@example.test"),
            ("First", "first@example.test;"),
            ("First", "fi<rst@example.test"),
        ] {
            assert!(
                !TaskGitAuthor {
                    name: name.into(),
                    email: email.into()
                }
                .valid_for_git()
            );
            assert_ne!(
                render(name, email),
                format!("{name} <{email}> 1700000000 +0000\n")
            );
        }
    }
}
