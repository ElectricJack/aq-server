use super::*;

fn id(n: u128) -> Uuid {
    Uuid::from_u128(n)
}

#[test]
fn scopes_round_trip_and_refuse_aliases() {
    for s in ["attempt:self", "task:self", "task:subtree", "project:self"] {
        let scope: Scope = s.parse().unwrap();
        assert_eq!(scope.to_string(), s);
    }
    let p = id(7);
    let scope: Scope = format!("project:{p}").parse().unwrap();
    assert_eq!(scope, Scope::Project(p));
    for bad in [
        "project:",
        "project:example",
        "project:00000000-0000-0000-0000-000000000000",
        "project:00000000-0000-0000-0000-00000000000A",
        "task:other",
        "*",
    ] {
        assert!(bad.parse::<Scope>().is_err(), "{bad}");
    }
    let g: Grant =
        serde_json::from_str(r#"{"operation":"task.read","scope":"project:self"}"#).unwrap();
    assert_eq!(g, Grant::new(op::TASK_READ, Scope::ProjectSelf));
    assert!(
        serde_json::from_str::<Grant>(r#"{"operation":"task.read","scope":"x","y":1}"#).is_err()
    );
}

#[test]
fn grant_sets_refuse_owner_only_unknown_and_duplicates() {
    assert!(GrantSet::new(vec![Grant::new("question.answer", Scope::ProjectSelf)]).is_err());
    assert!(GrantSet::new(vec![Grant::new("task.activate", Scope::ProjectSelf)]).is_err());
    assert!(
        GrantSet::new(vec![
            Grant::new(op::TASK_READ, Scope::ProjectSelf),
            Grant::new(op::TASK_READ, Scope::TaskSelf)
        ])
        .is_err()
    );
    let a = GrantSet::new(vec![
        Grant::new(op::TASK_READ, Scope::ProjectSelf),
        Grant::new(op::COMMENT_READ, Scope::ProjectSelf),
    ])
    .unwrap();
    let b = GrantSet::new(vec![
        Grant::new(op::COMMENT_READ, Scope::ProjectSelf),
        Grant::new(op::TASK_READ, Scope::ProjectSelf),
    ])
    .unwrap();
    assert_eq!(a.digest(), b.digest());
}

#[test]
fn scope_check_pins_project_and_subtree() {
    let (own, other) = (id(1), id(2));
    let h = Holder { project: own };
    let task = |relation| Target::Task {
        project: own,
        relation,
    };
    let foreign = Target::Task {
        project: other,
        relation: Relation::Other,
    };
    for (scope, target, permitted) in [
        (Scope::AttemptSelf, Target::OwnAttempt, true),
        (Scope::AttemptSelf, task(Relation::Own), false),
        (Scope::TaskSelf, task(Relation::Own), true),
        (Scope::TaskSelf, task(Relation::Descendant(1)), false),
        (Scope::TaskSubtree, task(Relation::Descendant(2)), true),
        (Scope::TaskSubtree, task(Relation::Other), false),
        (Scope::TaskSubtree, Target::Project(own), false),
        (Scope::ProjectSelf, task(Relation::Other), true),
        (Scope::ProjectSelf, Target::Project(own), true),
        (Scope::ProjectSelf, Target::Project(other), false),
        (Scope::ProjectSelf, foreign, false),
        (Scope::Project(other), foreign, true),
        (Scope::Project(other), task(Relation::Own), false),
    ] {
        assert_eq!(allows(scope, h, target), permitted, "{scope:?}: {target:?}");
    }
}

#[test]
fn check_names_the_missing_grant_and_held_family() {
    let project = id(1);
    let h = Holder { project };
    let grants = GrantSet::new(vec![
        Grant::new(op::TASK_CREATE_SUBTASK, Scope::TaskSubtree),
        Grant::new(op::TASK_READ, Scope::ProjectSelf),
        Grant::new(op::COMMENT_READ, Scope::ProjectSelf),
    ])
    .unwrap();
    let Decision::Deny(r) = check(
        &grants,
        h,
        GrantRequest::new(
            "task.create",
            op::TASK_CREATE_PROJECT,
            Target::Project(project),
        ),
    ) else {
        panic!()
    };
    assert_eq!(
        (r.code, r.grant.as_deref()),
        (RefusalCode::GrantMissing, Some("task.create.project"))
    );
    assert_eq!(r.held.len(), 2, "only the task.* family is shown");
    assert!(r.message.contains("task.create.project"));
    let body = r.body();
    assert_eq!(
        (&body["code"], &body["held"][0]["scope"]),
        (
            &serde_json::json!("grant_missing"),
            &serde_json::json!("task:subtree")
        )
    );
    let Decision::Deny(r) = check(
        &grants,
        h,
        GrantRequest::new(
            "task.create",
            op::TASK_CREATE_SUBTASK,
            Target::Task {
                project,
                relation: Relation::Other,
            },
        ),
    ) else {
        panic!()
    };
    assert_eq!(r.code, RefusalCode::ScopeDenied);
    let Decision::Deny(r) = check(
        &grants,
        h,
        GrantRequest::new("question.answer", "question.answer", Target::OwnAttempt),
    ) else {
        panic!()
    };
    assert_eq!(
        (r.code, r.code.legacy()),
        (RefusalCode::OwnerOnly, "unsupported_capability")
    );
    assert!(matches!(
        check(
            &grants,
            h,
            GrantRequest::new("task.show", op::TASK_READ, Target::Project(project))
        ),
        Decision::Allow(_)
    ));
}

#[test]
fn intersection_never_widens() {
    let creator = GrantSet::new(vec![
        Grant::new(op::TASK_CREATE_SUBTASK, Scope::TaskSubtree).limited(Limits {
            max_created_tasks: Some(5),
            max_depth: Some(2),
            draft_only: false,
        }),
        Grant::new(op::COMMENT_WRITE, Scope::TaskSelf),
    ])
    .unwrap();
    let child = GrantSet::new(vec![
        Grant::new(op::TASK_CREATE_SUBTASK, Scope::ProjectSelf).limited(Limits {
            max_created_tasks: Some(9),
            max_depth: None,
            draft_only: false,
        }),
        Grant::new(op::COMMENT_WRITE, Scope::ProjectSelf),
        Grant::new(op::TASK_CREATE_PROJECT, Scope::ProjectSelf),
    ])
    .unwrap();
    let got = child.intersect(&creator);
    assert!(got.get(op::TASK_CREATE_PROJECT).is_none());
    let sub = got.get(op::TASK_CREATE_SUBTASK).unwrap();
    assert_eq!(
        sub,
        &Grant::new(op::TASK_CREATE_SUBTASK, Scope::TaskSubtree).limited(Limits {
            max_created_tasks: Some(5),
            max_depth: Some(2),
            draft_only: false,
        })
    );
    assert_eq!(got.get(op::COMMENT_WRITE).unwrap().scope, Scope::TaskSelf);
    let other = GrantSet::new(vec![Grant::new(op::TASK_READ, Scope::Project(id(9)))]).unwrap();
    let mine = GrantSet::new(vec![Grant::new(op::TASK_READ, Scope::ProjectSelf)]).unwrap();
    assert!(other.intersect(&mine).grants().is_empty());
}

#[test]
fn every_route_grant_is_grantable() {
    let h = Holder { project: id(1) };
    for (path, command, grant) in AGENT_ROUTE_GRANTS {
        assert!(path.starts_with("/v1/agent/") && grantable(grant), "{path}");
        assert_eq!(
            route_grant(path, h, Relation::Own)
                .unwrap()
                .required
                .command,
            command
        );
    }
    assert!(matches!(
        route_grant("/v1/agent/comments", h, Relation::Own)
            .unwrap()
            .target,
        Target::Task {
            relation: Relation::Own,
            ..
        }
    ));
    // A comment list of another task is checked at that task's relation.
    assert!(matches!(
        route_grant("/v1/agent/comments/list", h, Relation::Other)
            .unwrap()
            .target,
        Target::Task {
            relation: Relation::Other,
            ..
        }
    ));
    for path in ["/v1/agent/waits/ack", "/v1/agent/waits/list"] {
        assert_eq!(
            route_grant(path, h, Relation::Other).unwrap().target,
            Target::OwnAttempt
        );
    }
    // Result reads are project targets: only a project scope reaches them.
    for path in [
        "/v1/agent/results/list",
        "/v1/agent/results/show",
        "/v1/agent/results/patch",
    ] {
        let target = route_grant(path, h, Relation::Own).unwrap().target;
        assert_eq!(target, Target::Project(id(1)));
        for (scope, allowed) in [
            (Scope::ProjectSelf, true),
            (Scope::Project(id(1)), true),
            (Scope::Project(id(2)), false),
            (Scope::TaskSubtree, false),
            (Scope::TaskSelf, false),
            (Scope::AttemptSelf, false),
        ] {
            assert_eq!(allows(scope, h, target), allowed, "{path} {scope}");
        }
    }
    assert!(route_grant("/v1/agent/messages/send", h, Relation::Own).is_none());
    for o in OWNER_ONLY {
        assert!(!grantable(o));
    }
}

#[test]
fn vocabulary_matches_published_contract() {
    let schema: serde_json::Value =
        serde_json::from_str(include_str!("../../../protocol/commands/v1/schema.json")).unwrap();
    let ops: Vec<&str> = schema["$defs"]["Operation"]["enum"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect();
    // The published vocabulary is the default build's (spec 0017 adds one).
    #[cfg(feature = "project_environments")]
    let ops = [ops, vec![op::ENVIRONMENT_TRY]].concat();
    assert_eq!(ops, op::ALL);
    let codes = schema["$defs"]["RefusalCode"]["enum"].as_array().unwrap();
    for code in CODES {
        assert!(codes.contains(&serde_json::json!(code.as_str())));
        assert_eq!(serde_json::to_value(code).unwrap(), code.as_str());
    }
    assert_eq!(codes.len(), CODES.len());
}

#[test]
fn published_fixtures_are_what_the_crate_produces() {
    let fixture: Refusal = serde_json::from_str(include_str!(
        "../../../protocol/commands/v1/fixtures/refusal-grant-missing.json"
    ))
    .unwrap();
    let held = fixture.held.clone();
    let made = Refusal::grant_missing(
        RequiredGrant::new("task.create", op::TASK_CREATE_PROJECT),
        held,
    )
    .with_request(fixture.request_id.unwrap());
    assert_eq!(made, fixture);
    let fixture: Refusal = serde_json::from_str(include_str!(
        "../../../protocol/commands/v1/fixtures/refusal-scope-denied.json"
    ))
    .unwrap();
    let made = Refusal::scope_denied(
        RequiredGrant::new("comment.list", op::COMMENT_READ),
        fixture.held.clone(),
    )
    .with_request(fixture.request_id.unwrap());
    assert_eq!(made, fixture);
}

#[test]
fn default_coding_grants_follow_the_owner_decisions() {
    let d = resolve::default_coding();
    let sub = d.get(op::TASK_CREATE_SUBTASK).unwrap();
    assert_eq!(sub.scope, Scope::TaskSubtree);
    assert_eq!(
        (sub.limits.max_created_tasks, sub.limits.max_depth),
        (Some(5), Some(2))
    );
    for (operation, scope) in [
        (op::KNOWLEDGE_READ, Some(Scope::ProjectSelf)),
        (op::TASK_CREATE_PROJECT, None),
        (op::TASK_EDIT, None),
        (op::MESSAGE_SEND, Some(Scope::TaskSubtree)),
        (op::QUESTION_ASK, Some(Scope::AttemptSelf)),
    ] {
        assert_eq!(d.get(operation).map(|g| g.scope), scope, "{operation}");
    }
}

#[test]
fn resolution_applies_profile_ceiling_additions_and_inheritance() {
    use resolve::{Permission, ProjectGrants, from_permissions, resolve};
    let project = id(4);
    let perms = [
        Permission {
            operation: "task.read",
            scope_reference: &format!("project:{project}"),
        },
        Permission {
            operation: "wait.show",
            scope_reference: "attempt:self",
        },
        Permission {
            operation: "source.write",
            scope_reference: "repository:x",
        },
        Permission {
            operation: "task.edit",
            scope_reference: &format!("project:{}", id(5)),
        },
    ];
    let profile = from_permissions(&perms, project);
    assert_eq!(
        profile,
        vec![
            Grant::new(op::TASK_READ, Scope::ProjectSelf),
            Grant::new(op::WAIT_READ, Scope::AttemptSelf)
        ]
    );
    let plain = resolve(&profile, &ProjectGrants::default(), None);
    assert_eq!(plain, resolve::default_coding());
    // The owner adds top-level creation for this project.
    let widened = ProjectGrants {
        ceiling: None,
        by_work_kind: Default::default(),
        add: vec![Grant::new(op::TASK_CREATE_PROJECT, Scope::ProjectSelf)],
    };
    let w = resolve(&profile, &widened, None);
    assert!(w.get(op::TASK_CREATE_PROJECT).is_some());
    // A ceiling narrows: no knowledge authoring, comments only on the own task.
    let narrow = ProjectGrants {
        ceiling: Some(vec![
            Grant::new(op::TASK_READ, Scope::ProjectSelf),
            Grant::new(op::COMMENT_WRITE, Scope::TaskSelf),
            Grant::new(op::TASK_CREATE_SUBTASK, Scope::TaskSubtree).limited(Limits {
                max_created_tasks: Some(1),
                max_depth: None,
                draft_only: false,
            }),
        ]),
        add: vec![],
        by_work_kind: Default::default(),
    };
    let n = resolve(&profile, &narrow, None);
    assert_eq!(n.grants().len(), 3);
    assert_eq!(n.get(op::COMMENT_WRITE).unwrap().scope, Scope::TaskSelf);
    assert_eq!(
        n.get(op::TASK_CREATE_SUBTASK).unwrap().limits,
        Limits {
            max_created_tasks: Some(1),
            max_depth: Some(2),
            draft_only: false
        }
    );
    // A child of an agent never exceeds its creator, even with an addition.
    let child = resolve(&profile, &widened, Some(&n));
    assert!(child.get(op::TASK_CREATE_PROJECT).is_none());
    assert!(child.grants().len() <= n.grants().len());
}

#[test]
fn only_a_rejected_credential_is_unauthorized() {
    for (code, refusal, status) in [
        ("unauthorized", RefusalCode::Unauthorized, 401),
        ("stale_generation", RefusalCode::Unauthorized, 401),
        ("session_revoked", RefusalCode::SessionRevoked, 401),
        ("session_expired", RefusalCode::SessionExpired, 401),
        (
            "unsupported_api_version",
            RefusalCode::UnsupportedApiVersion,
            426,
        ),
        ("overloaded", RefusalCode::Overloaded, 503),
        ("internal", RefusalCode::Internal, 500),
        ("invalid_request", RefusalCode::Internal, 500),
    ] {
        assert_eq!(RefusalCode::from_agent_access(code), refusal, "{code}");
        assert_eq!(refusal.status(), status, "{code}");
    }
}

#[test]
fn parent_child_edges_follow_the_task_read_scope() {
    let set = |scope: &str| {
        GrantSet::new(vec![
            serde_json::from_value(serde_json::json!({"operation":"task.read","scope":scope}))
                .unwrap(),
        ])
        .unwrap()
    };
    let (pc, refs) = (
        serde_json::json!("parent-child"),
        serde_json::json!("references"),
    );
    let profile = vec![refs.clone()];
    let empty = GrantSet::empty();
    // The profile's own types are readable whatever the grants.
    assert!(empty.edge_readable(Some(&profile), &refs, Some((false, false))));
    // parent-child outside the profile needs task.read reaching the parent.
    assert!(!empty.edge_readable(Some(&profile), &pc, None));
    for (scope, own, below, readable) in [
        ("project:self", false, false, true),
        ("task:subtree", true, false, true),
        ("task:subtree", false, true, true),
        ("task:subtree", false, false, false),
        ("task:self", true, false, true),
        ("task:self", false, true, false),
    ] {
        let g = set(scope);
        assert!(
            g.edge_readable(Some(&profile), &pc, None),
            "{scope}: type check"
        );
        assert_eq!(
            g.edge_readable(Some(&profile), &pc, Some((own, below))),
            readable,
            "{scope} own={own} below={below}"
        );
        assert!(!g.edge_readable(
            Some(&profile),
            &serde_json::json!("blocks"),
            Some((true, true))
        ));
    }
    assert!(set("task:subtree").reads_subtree() && !set("task:self").reads_subtree());
}

#[test]
fn work_kind_policies_set_draft_grants_without_changing_coding_defaults() {
    use super::resolve::{ProjectGrants, default_coding, resolve_work};
    let policies: ProjectGrants = serde_json::from_value(serde_json::json!({"by_work_kind":{
        "investigation":{"max_created_tasks":0},
        "spec_ingestion":{"max_created_tasks":30},
        "fidelity_review":{"max_created_tasks":10,"edit_drafts":true}
    }, "add":[{"operation":"task.create.project","scope":"project:self"}]}))
    .unwrap();
    policies.validate().unwrap();
    let grants = |kind| resolve_work(&[], &policies, None, Some(kind));
    let investigator = grants("investigation");
    assert!(investigator.get(op::KNOWLEDGE_AUTHOR).is_some());
    assert!(investigator.get(op::TASK_CREATE_SUBTASK).is_none());
    assert!(investigator.get(op::TASK_CREATE_PROJECT).is_none());
    for (kind, max) in [("spec_ingestion", 30), ("fidelity_review", 10)] {
        let set = grants(kind);
        let creation = set.get(op::TASK_CREATE_SUBTASK).unwrap();
        assert_eq!(
            (
                creation.limits.draft_only,
                creation.limits.max_created_tasks,
                creation.scope
            ),
            (true, Some(max), Scope::TaskSubtree)
        );
        assert!(set.get(op::TASK_CREATE_PROJECT).is_none());
        for operation in [op::TASK_EDIT, op::TASK_CANCEL] {
            assert_eq!(
                set.get(operation).is_some(),
                kind == "fidelity_review",
                "{operation}"
            );
        }
    }
    let child = resolve_work(
        &[],
        &policies,
        Some(&grants("spec_ingestion")),
        Some("coding"),
    );
    assert!(child.get(op::TASK_CREATE_SUBTASK).is_none());
    let other = grants("coding");
    assert_eq!(
        other.get(op::TASK_CREATE_SUBTASK),
        default_coding().get(op::TASK_CREATE_SUBTASK)
    );
    assert!(other.get(op::TASK_CREATE_PROJECT).is_some());
    let narrowed = policies
        .work_kind(Some("fidelity_review"))
        .unwrap()
        .narrow(&grants("spec_ingestion"));
    assert_eq!(
        narrowed
            .get(op::TASK_CREATE_SUBTASK)
            .unwrap()
            .limits
            .max_created_tasks,
        Some(10)
    );
    assert!(narrowed.get(op::TASK_EDIT).is_none());
    assert!(
        policies
            .work_kind(Some("spec_ingestion"))
            .unwrap()
            .narrow(&other)
            .get(op::TASK_CREATE_SUBTASK)
            .is_none()
    );
}
