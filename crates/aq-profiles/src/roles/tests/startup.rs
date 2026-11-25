use super::*;

fn startup<'a>(role: &'a Role) -> StartupInput<'a> {
    StartupInput {
        platform: "Platform.",
        role,
        project_notes: Some("Project notes."),
        repository_notes: Some("Repository notes."),
        task: "Task é.",
        task_thread: None,
        reporting: Some("Report."),
    }
}
#[test]
fn startup_text_has_a_fixed_order_and_double_newline_joining() {
    let role = parse_role(ROLE).unwrap();
    let text = assemble_startup(&startup(&role), 65_536).unwrap();
    assert_eq!(
        text.instructions
            .iter()
            .map(|(part, _)| *part)
            .collect::<Vec<_>>(),
        [
            StartupPart::Platform,
            StartupPart::Role,
            StartupPart::Rules,
            StartupPart::ProjectNotes,
            StartupPart::RepositoryNotes,
            StartupPart::Task,
            StartupPart::Reporting,
        ]
    );
    assert_eq!(
        text.rendered,
        "Platform.\n\n## Role\nYou implement the change your task describes.\n\n```md\n## Not a heading\n```\n\n## Rules\n- Change only what the task asks for.\n\nProject notes.\n\nRepository notes.\n\nTask é.\n\nReport."
    );
    // The same join the renderer applies to the instruction list.
    let joined = text
        .instructions
        .iter()
        .map(|(_, t)| t.as_str())
        .collect::<Vec<_>>()
        .join("\n\n");
    assert_eq!(text.rendered, joined);

    let mut bare = role.clone();
    bare.rules_md.clear();
    let input = StartupInput {
        project_notes: None,
        repository_notes: Some(" \n"),
        ..startup(&bare)
    };
    let text = assemble_startup(&input, 65_536).unwrap();
    assert_eq!(
        text.instructions
            .iter()
            .map(|(part, _)| *part)
            .collect::<Vec<_>>(),
        [
            StartupPart::Platform,
            StartupPart::Role,
            StartupPart::Task,
            StartupPart::Reporting,
        ]
    );
}
#[test]
fn startup_text_over_the_limit_is_refused_not_trimmed() {
    let role = parse_role(ROLE).unwrap();
    let input = startup(&role);
    let bytes = assemble_startup(&input, u64::MAX).unwrap().rendered.len() as u64;
    assert!(
        bytes
            > assemble_startup(&input, u64::MAX)
                .unwrap()
                .rendered
                .chars()
                .count() as u64
    );
    assemble_startup(&input, bytes).unwrap();
    let error = assemble_startup(&input, bytes - 1).unwrap_err();
    assert_eq!(
        error,
        StartupError::OverLimit {
            bytes,
            limit: bytes - 1
        }
    );
    assert_eq!(
        error.to_string(),
        format!(
            "the startup text is {bytes} bytes, over the execution profile's limit of {} bytes",
            bytes - 1
        )
    );
    for (part, input) in [
        (
            StartupPart::Platform,
            StartupInput {
                platform: " ",
                ..input
            },
        ),
        (StartupPart::Task, StartupInput { task: "", ..input }),
        (
            StartupPart::Reporting,
            StartupInput {
                task_thread: None,
                reporting: Some("\n"),
                ..input
            },
        ),
    ] {
        assert_eq!(
            assemble_startup(&input, u64::MAX).unwrap_err(),
            StartupError::Empty(part)
        );
    }
    assert_eq!(
        StartupError::Empty(StartupPart::Task).to_string(),
        "the task is empty"
    );
}
