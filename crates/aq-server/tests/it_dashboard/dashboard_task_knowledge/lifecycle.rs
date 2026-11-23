use super::*;

#[tokio::test]
async fn task_retry_creates_replacement_attempt() {
    let x = setup().await;
    let pool = x.f.db.pool().await;
    let task = x.f.selection.lineage.task_id;
    let predecessor = x.f.profiled.offer.binding.attempt_id;
    x.put(node(task, x.workspace(), "task", 1));
    let hot = seed_offered_predecessor(&x, &pool, predecessor).await;
    let holds = || async {
        sqlx::query_scalar::<_, bool>(
            "SELECT holds_task FROM execution_attempts WHERE attempt_id=$1",
        )
        .bind(predecessor)
        .fetch_one(&pool)
        .await
        .unwrap()
    };
    let retry = |op: Uuid| {
        action(
            "task.retry",
            Subject::Task(task, "1"),
            json!({"operation_id":op,"project_id":x.project(),"expected_version":"1"}),
        )
    };

    let operation = Uuid::new_v4();
    let confirmed_retry = retry(operation);
    let (status, body) = x.act(&confirmed_retry).await;
    assert_eq!(status, StatusCode::ACCEPTED, "{body}");
    assert_eq!(body["data"]["disposition"], "pending", "{body}");
    assert!(holds().await, "pending retry preserves the original claim");
    let wait = aq_human_work::owner_retry_wait(&pool, x.project(), task)
        .await
        .unwrap();
    assert_eq!(wait["operation_id"], json!(operation));
    assert_eq!(wait["attempt_id"], json!(predecessor));
    assert_eq!(wait["hold_reason"], "replacement_prior_execution_uncertain");
    assert_eq!(wait["blocker_owner"], "aq_worker");
    assert_eq!(wait["wake_condition"], "prior_execution_verified");
    assert!(wait["next_check_at_ms"].as_i64().is_some());

    settle_predecessor(&x, &pool, predecessor, hot).await;
    link_source_export(&x, &pool, predecessor).await;
    let schedule = || async {
        sqlx::query("UPDATE owner_task_retry_intents SET next_check_at=to_timestamp(1) WHERE operation_id=$1")
            .bind(operation).execute(&pool).await.unwrap();
    };
    let wait_for_check = async |n| {
        tokio::time::timeout(std::time::Duration::from_secs(12), async {
            loop {
                let count: i32 = sqlx::query_scalar(
                    "SELECT checks FROM owner_task_retry_intents WHERE operation_id=$1",
                )
                .bind(operation)
                .fetch_one(&pool)
                .await
                .unwrap();
                if count >= n {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            }
        })
        .await
        .expect("AQ checked the original approval");
    };
    sqlx::raw_sql("CREATE FUNCTION fail_retry_receipt() RETURNS trigger LANGUAGE plpgsql AS $$
      BEGIN IF NEW.kind='task.retry' AND NEW.disposition='confirmed' THEN RAISE EXCEPTION 'lost acknowledgement'; END IF;
      RETURN NEW; END $$;
      CREATE TRIGGER fail_retry_receipt BEFORE UPDATE ON dashboard_operations
      FOR EACH ROW EXECUTE FUNCTION fail_retry_receipt();")
        .execute(&pool).await.unwrap();
    let set_role = async |role| {
        sqlx::query("UPDATE users SET role=$2 WHERE id=(SELECT principal_id FROM dashboard_operations WHERE operation_id=$1)")
            .bind(operation).bind(role).execute(&pool).await.unwrap();
    };
    set_role("viewer").await;
    schedule().await;
    let reason = aq_human_work::owner_retry_wait(&pool, x.project(), task)
        .await
        .unwrap();
    assert_eq!(reason["operation_id"], json!(operation));
    assert_eq!(
        reason["wake_condition"],
        "original_retry_authority_restored"
    );

    let (stop, receiver) = tokio::sync::watch::channel(false);
    let sweep = tokio::spawn(aq_server::profiled_execution::expiry::run(
        x.f.state.clone(),
        receiver,
    ));
    wait_for_check(1).await;
    assert!(
        holds().await,
        "retry remains held while owner permission is missing"
    );
    set_role("owner").await;
    schedule().await;
    wait_for_check(2).await;
    tokio::time::timeout(std::time::Duration::from_secs(12), async {
        while holds().await {
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
    })
    .await
    .expect(
        "AQ releases the same predecessor after its evidence and the approver's permission return",
    );

    sqlx::query("DROP TRIGGER fail_retry_receipt ON dashboard_operations")
        .execute(&pool)
        .await
        .unwrap();
    schedule().await;
    let body: Value = tokio::time::timeout(std::time::Duration::from_secs(12), async {
        loop {
            let receipt: Value = sqlx::query_scalar(
                "SELECT response FROM dashboard_operations WHERE operation_id=$1",
            )
            .bind(operation)
            .fetch_one(&pool)
            .await
            .unwrap();
            if receipt["disposition"] == "confirmed" {
                break json!({"data":receipt});
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
    })
    .await
    .expect("AQ completes the original operation without another request");
    let _ = stop.send(true);
    sweep.await.unwrap();
    assert_eq!(
        body["data"]["receipt_refs"][0],
        json!({"type":"replacement_release","id":predecessor})
    );
    assert!(!holds().await, "B4 released the failed predecessor");
    assert_replacement_ready(&x, &pool, task, predecessor).await;
    assert!(
        x.posts().is_empty(),
        "retry never writes Graph task lifecycle"
    );
    // Review M-3: once the task is held (here quarantined), a NEW retry is refused with
    // the hold, but a replay of the admitted operation keeps its recorded outcome.
    sqlx::query("INSERT INTO stall_quarantines(attempt_id,lineage_id,reason,escalation,held,action_id,created_at_ms) VALUES($1,$2,'synthetic stall','owner','{}',$3,1)")
        .bind(predecessor).bind(task).bind(Uuid::new_v4())
        .execute(&pool).await.unwrap();
    let (status, body) = x.act(&retry(Uuid::new_v4())).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(body["code"], "stall_quarantined", "{body}");
    let (status, body) = x.act(&confirmed_retry).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["data"]["disposition"], "confirmed", "{body}");
}

#[tokio::test]
async fn owner_activates_a_draft_task_and_only_a_draft() {
    let x = setup().await;
    let request = |id: Uuid| {
        let op = Uuid::new_v4();
        action(
            "task.activate",
            Subject::Task(id, "3"),
            json!({"operation_id":op,"project_id":x.project(),"expected_version":"3"}),
        )
    };
    // A draft becomes active with one Graph write at the expected version.
    let id = Uuid::new_v4();
    let mut before = node(id, x.workspace(), "task", 3);
    before["task_state"] = json!("draft");
    x.put(before.clone());
    let mut after = before;
    after["version"] = json!(4);
    after["task_state"] = json!("active");
    x.reply(200, change_set(vec![after]));
    let activate = request(id);
    let (status, body) = x.act(&activate).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["data"]["disposition"], "confirmed", "{body}");
    let native = x.posts().last().unwrap()["operations"][0].clone();
    assert_eq!(
        (
            native["op"].clone(),
            native["expected_version"].clone(),
            native["patch"].clone()
        ),
        (
            json!("update_node"),
            json!(3),
            json!({"task_state":"active"})
        )
    );
    // Replaying the operation returns its result and writes nothing more.
    let posts = x.posts().len();
    let (status, replay) = x.act(&activate).await;
    assert_eq!(status, StatusCode::OK, "{replay}");
    assert_eq!(replay["data"]["disposition"], "confirmed", "{replay}");
    assert_eq!(x.posts().len(), posts);
    // Any other state is rejected before Graph is asked to change anything:
    // reopening a finished task is not activation.
    for state in ["active", "completed", "canceled"] {
        let id = Uuid::new_v4();
        let mut task = node(id, x.workspace(), "task", 3);
        task["task_state"] = json!(state);
        x.put(task);
        let posts = x.posts().len();
        let (status, body) = x.act(&request(id)).await;
        assert_eq!(
            body["data"]["disposition"], "rejected",
            "{state}: {status} {body}"
        );
        assert_eq!(x.posts().len(), posts, "{state}");
    }
}

#[tokio::test]
async fn owner_reopens_a_finished_task_only_if_it_never_ran() {
    let x = setup().await;
    let request = |id: Uuid| {
        let op = Uuid::new_v4();
        action(
            "task.reopen",
            Subject::Task(id, "3"),
            json!({"operation_id":op,"project_id":x.project(),"expected_version":"3"}),
        )
    };
    for state in ["completed", "canceled"] {
        let id = Uuid::new_v4();
        let mut before = node(id, x.workspace(), "task", 3);
        before["task_state"] = json!(state);
        x.put(before.clone());
        let mut after = before;
        after["version"] = json!(4);
        after["task_state"] = json!("active");
        x.reply(200, change_set(vec![after]));
        let (status, body) = x.act(&request(id)).await;
        assert_eq!(status, StatusCode::OK, "{state}: {body}");
        assert_eq!(body["data"]["disposition"], "confirmed", "{state}: {body}");
        let native = x.posts().last().unwrap()["operations"][0].clone();
        assert_eq!(native["patch"], json!({"task_state":"active"}), "{state}");
    }
    // A task that is not finished is not reopened.
    for state in ["draft", "active"] {
        let id = Uuid::new_v4();
        let mut task = node(id, x.workspace(), "task", 3);
        task["task_state"] = json!(state);
        x.put(task);
        let posts = x.posts().len();
        let (_, body) = x.act(&request(id)).await;
        assert_eq!(body["data"]["disposition"], "rejected", "{state}: {body}");
        assert_eq!(x.posts().len(), posts, "{state}");
    }
    // The fixture's task has an attempt: finished or not, it is not reopened.
    let ran = x.f.selection.lineage.task_id;
    let mut task = node(ran, x.workspace(), "task", 3);
    task["task_state"] = json!("completed");
    x.put(task);
    let posts = x.posts().len();
    let (_, body) = x.act(&request(ran)).await;
    assert_eq!(body["data"]["disposition"], "rejected", "{body}");
    assert_eq!(x.posts().len(), posts);
}

#[tokio::test]
async fn owner_completes_a_task_by_hand_only_when_nothing_else_owns_its_outcome() {
    let x = setup().await;
    let request = |id: Uuid, reason: Option<&str>| {
        let op = Uuid::new_v4();
        let mut request = action(
            "task.complete",
            Subject::Task(id, "3"),
            json!({"operation_id":op,"project_id":x.project(),"expected_version":"3"}),
        );
        if let Some(reason) = reason {
            request["reason"] = json!(reason);
        }
        request
    };
    let put = |state: &str, metadata: Value| {
        let id = Uuid::new_v4();
        let mut task = node(id, x.workspace(), "task", 3);
        task["task_state"] = json!(state);
        task["metadata"] = metadata;
        x.put(task.clone());
        (id, task)
    };
    // An active task nothing holds is completed with one Graph write that
    // carries the owner's reason.
    let (id, before) = put("active", json!({}));
    let mut after = before;
    after["version"] = json!(4);
    after["task_state"] = json!("completed");
    x.reply(200, change_set(vec![after]));
    let (status, body) = x.act(&request(id, Some("Done by hand"))).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["data"]["disposition"], "confirmed", "{body}");
    let posted = x.posts().last().unwrap().clone();
    assert_eq!(posted["reason"], "Done by hand");
    assert_eq!(
        posted["operations"][0]["patch"],
        json!({"task_state":"completed"})
    );
    // Without a reason nothing is admitted.
    let (id, _) = put("active", json!({}));
    let posts = x.posts().len();
    let (status, body) = x.act(&request(id, None)).await;
    assert!(status.is_client_error(), "{status} {body}");
    let (status, body) = x.act(&request(id, Some("  "))).await;
    assert!(status.is_client_error(), "{status} {body}");
    assert_eq!(x.posts().len(), posts);
    // Rejected, with nothing written: not active; managed by a playbook;
    // held by an attempt (the fixture's task has one).
    let (draft, _) = put("draft", json!({}));
    let (done, _) = put("completed", json!({}));
    let (managed, _) = put("active", json!({"aq:playbook_namespace":"ns"}));
    let held = x.f.selection.lineage.task_id;
    x.put(node(held, x.workspace(), "task", 3));
    for (what, id) in [
        ("draft", draft),
        ("completed", done),
        ("playbook", managed),
        ("held", held),
    ] {
        let posts = x.posts().len();
        let (_, body) = x.act(&request(id, Some("Done by hand"))).await;
        assert_eq!(body["data"]["disposition"], "rejected", "{what}: {body}");
        assert_eq!(x.posts().len(), posts, "{what}");
    }
    // Graph refuses a parent with an open child as a conflict at the same
    // version: that is a rejection, not a stale version to retry.
    let (parent, _) = put("active", json!({}));
    x.reply(
        409,
        json!({"error":{"code":"conflict","message":"open child prevents parent completion"}}),
    );
    let (_, body) = x.act(&request(parent, Some("Done by hand"))).await;
    assert_eq!(body["data"]["disposition"], "rejected", "{body}");
}

#[tokio::test]
async fn owner_cancels_tasks_without_reason_or_attempt_and_deletes_without_gate() {
    let x = setup().await;
    for (state, kind) in [
        ("draft", "task.cancel"),
        ("active", "task.cancel"),
        ("completed", "task.delete"),
        ("canceled", "task.delete"),
    ] {
        let id = Uuid::new_v4();
        let mut before = node(id, x.workspace(), "task", 3);
        before["task_state"] = json!(state);
        if kind == "task.delete" {
            before["retired"] = json!(true);
            before["retirement_reason"] = json!("Owner retired task");
        }
        x.put(before.clone());
        let mut after = before;
        after["version"] = json!(4);
        if kind == "task.delete" {
            after["deleted"] = json!(true);
        } else {
            after["task_state"] = json!("canceled");
        }
        x.reply(200, change_set(vec![after]));
        let op = Uuid::new_v4();
        let request = action(
            kind,
            Subject::Task(id, "3"),
            json!({"operation_id":op,"project_id":x.project(),"expected_version":"3"}),
        );
        let (status, body) = x.act(&request).await;
        assert_eq!(
            status,
            StatusCode::OK,
            "{state} {kind} {body}; native posts: {:?}",
            x.posts()
        );
        assert_eq!(body["data"]["disposition"], "confirmed", "{body}");
        let native = x.posts().last().unwrap()["operations"][0].clone();
        assert_eq!(
            native["op"],
            if kind == "task.delete" {
                "delete_node"
            } else {
                "update_node"
            }
        );
        if kind == "task.cancel" {
            assert_eq!(native["patch"]["task_state"], "canceled");
        }
        let (status, replay) = x.act(&request).await;
        assert_eq!(status, StatusCode::OK, "{replay}");
        assert_eq!(replay["data"], body["data"]);
    }
}

#[tokio::test]
async fn canceled_task_cleanup_retries_after_restart_and_releases_only_after_actual_stop() {
    use aq_server::profiled_execution::{started, stopped};
    use common::native_started;
    let (f, start) = native_started::setup(true).await;
    let auth = native_started::context(&f).await;
    started::record(&f.state, &auth, &start).await.unwrap();
    let x = setup_runtime(f).await;
    let task = x.f.selection.lineage.task_id;
    let before = node(task, x.workspace(), "task", 3);
    x.put(before.clone());
    let mut after = before;
    after["version"] = json!(4);
    after["task_state"] = json!("canceled");
    x.reply(200, change_set(vec![after]));
    let op = Uuid::new_v4();
    let request = action(
        "task.cancel",
        Subject::Task(task, "3"),
        json!({"operation_id":op,"project_id":x.project(),"expected_version":"3"}),
    );
    let (status, receipt) = x.act(&request).await;
    assert_eq!(status, 200, "{receipt}");
    assert_eq!(receipt["data"]["disposition"], "confirmed");
    let mut sweep = aq_server::agent_access::human::task_termination::Sweep::default();
    sweep.tick(&x.f.state).await.unwrap();
    let pool = x.f.db.pool().await;
    let (held,cancels):(bool,i64) = sqlx::query_as("SELECT holds_task,(SELECT count(*) FROM profiled_cancellations WHERE attempt_id=$1) FROM execution_attempts WHERE attempt_id=$1").bind(start.binding.attempt_id).fetch_one(&pool).await.unwrap();
    assert!(held, "a cancel request is not a stop proof");
    assert_eq!(
        cancels, 1,
        "background cleanup must request the native stop"
    );
    let mut stop = native_started::stop_request(&start);
    stop.evidence.reason = aq_admission::transport::StopReason::Cancellation;
    stopped::record(&x.f.state, &auth, &stop).await.unwrap();
    // A new cursor models a server restart; durable owner intent is sufficient.
    let mut restarted = aq_server::agent_access::human::task_termination::Sweep::default();
    restarted.tick(&x.f.state).await.unwrap();
    let (held,dispositions):(bool,i64) = sqlx::query_as("SELECT holds_task,(SELECT count(*) FROM profiled_stopped_dispositions WHERE attempt_id=$1) FROM execution_attempts WHERE attempt_id=$1").bind(start.binding.attempt_id).fetch_one(&pool).await.unwrap();
    assert!(!held, "verified stopped cancellation must not stay held");
    assert_eq!(dispositions, 1);
    restarted.tick(&x.f.state).await.unwrap();
}

#[tokio::test]
async fn subtree_activation_activates_four_and_skips_active_branch() {
    let x = setup().await;
    let fixture = activation_fixture(&x);
    let (_, body) = x.act(&fixture.request).await;
    assert_eq!(body["data"]["disposition"], "confirmed", "{body}");
    assert_eq!(
        body["data"]["activation"],
        json!({"activated":4,"skipped":1})
    );
    assert_eq!(
        body["data"]["affected_entities"].as_array().unwrap().len(),
        4
    );
    for draft in fixture.drafts {
        let live = &x.graph.nodes.lock().unwrap()[&draft];
        assert_eq!(live["task_state"], "active");
        assert_eq!(live["version"], 6);
    }
    let (_, replay) = x.act(&fixture.request).await;
    assert_eq!(replay["data"], body["data"]);
    let nodes = x.graph.nodes.lock().unwrap();
    assert_eq!(nodes[&fixture.root]["task_state"], "draft");
    assert_eq!(nodes[&fixture.root]["version"], 3);
    assert_eq!(nodes[&fixture.branch]["task_state"], "active");
    assert_eq!(nodes[&fixture.branch]["version"], 2);
    let posts = x.posts();
    assert_eq!(posts.len(), 1);
    assert_eq!(posts[0]["operations"].as_array().unwrap().len(), 6);
}

#[tokio::test]
async fn subtree_activation_conflict_leaves_every_draft_unchanged() {
    let x = setup().await;
    let fixture = activation_fixture(&x);
    // Changing a nested non-draft parent's links bumps its version before
    // Graph accepts the batch. All the planned draft changes must roll back.
    x.graph
        .nodes
        .lock()
        .unwrap()
        .get_mut(&fixture.branch)
        .unwrap()["version"] = json!(3);
    let (_, body) = x.act(&fixture.request).await;
    assert_eq!(body["data"]["disposition"], "conflict", "{body}");
    assert_eq!(x.posts().len(), 1);
    for draft in fixture.drafts {
        let live = &x.graph.nodes.lock().unwrap()[&draft];
        assert_eq!(live["task_state"], "draft");
        assert_eq!(live["version"], 5);
    }
}

struct ActivationFixture {
    root: Uuid,
    branch: Uuid,
    drafts: [Uuid; 4],
    request: Value,
}

fn activation_fixture(x: &Fixture) -> ActivationFixture {
    let root = Uuid::new_v4();
    let branch = Uuid::new_v4();
    let drafts = std::array::from_fn(|_| Uuid::new_v4());
    let task = |id, state, version| {
        let mut value = node(id, x.workspace(), "task", version);
        value["task_state"] = json!(state);
        x.put(value.clone());
        value
    };
    let tree = |task: Value, children: Vec<Value>| json!({"node":task,"children":children,"truncated":false});
    let draft = |id| tree(task(id, "draft", 5), vec![]);
    *x.graph.tree.lock().unwrap() = Some(tree(
        task(root, "draft", 3),
        vec![
            tree(task(branch, "active", 2), vec![draft(drafts[0])]),
            draft(drafts[1]),
            draft(drafts[2]),
            draft(drafts[3]),
        ],
    ));
    ActivationFixture {
        root,
        branch,
        drafts,
        request: action(
            "task.activate.subtree",
            Subject::Task(root, "3"),
            json!({"operation_id":Uuid::new_v4(),"project_id":x.project(),"expected_version":"3"}),
        ),
    }
}

async fn seed_offered_predecessor(
    x: &Fixture,
    pool: &sqlx::PgPool,
    predecessor: Uuid,
) -> aq_server::execution::authority::hot::HotState {
    use aq_server::execution::authority::hot::HotState;
    // Synthetic B4 history: the fixture launch was offered at NOW.
    sqlx::query("ALTER TABLE profiled_launches DISABLE TRIGGER profiled_launch_guard")
        .execute(pool)
        .await
        .unwrap();
    sqlx::query("UPDATE profiled_launches SET graph_task_observation=jsonb_set(graph_task_observation,'{native_offer}',jsonb_build_object('offered',jsonb_build_object('clock',jsonb_build_object('server_time_ms',$2::BIGINT)))) WHERE attempt_id=$1")
        .bind(predecessor).bind(common::profiled::NOW as i64).execute(pool).await.unwrap();
    sqlx::query("ALTER TABLE profiled_launches ENABLE TRIGGER profiled_launch_guard")
        .execute(pool)
        .await
        .unwrap();
    let hot = HotState::from_compat(&x.f.profiled).unwrap();
    sqlx::query("INSERT INTO profiled_authority_hot(attempt_id,state,record_revision,authority_revision,grant_revision) VALUES($1,$2,1,1,0)").bind(predecessor).bind(json!(hot)).execute(pool).await.unwrap();
    hot
}

async fn settle_predecessor(
    x: &Fixture,
    pool: &sqlx::PgPool,
    predecessor: Uuid,
    mut hot: aq_server::execution::authority::hot::HotState,
) {
    use aq_server::execution::{
        authority::hot::LaunchOwner,
        profiled::{Effects, Retention, Status, Stop},
    };
    hot.status = Status::Finished;
    hot.owner = Some(LaunchOwner {
        guardian_id: Uuid::new_v4(),
        launch_id: Uuid::new_v4(),
        reservation_id: Uuid::new_v4(),
    });
    hot.stop = Stop::Stopped {
        receipt: common::profiled::artifact("stop", b"stop"),
    };
    hot.retention = Retention::Retained {
        receipt: common::profiled::artifact("retained", b"retained"),
    };
    hot.effects = Effects::Quiescent {
        receipt: common::profiled::artifact("effects", b"effects"),
    };
    hot.validate_against(&x.f.profiled.offer.binding).unwrap();
    sqlx::query("UPDATE profiled_authority_hot SET state=$2 WHERE attempt_id=$1")
        .bind(predecessor)
        .bind(json!(hot))
        .execute(pool)
        .await
        .unwrap();
}

async fn link_source_export(x: &Fixture, pool: &sqlx::PgPool, predecessor: Uuid) {
    // Retry decides as the offer path does (M2), which also requires that the
    // predecessor's input pins the current project revision: a synthetic
    // linked source export (guards bypassed in this disposable database).
    let revision: i64 =
        sqlx::query_scalar("SELECT revision FROM project_execution_bindings WHERE project_id=$1")
            .bind(x.project())
            .fetch_one(pool)
            .await
            .unwrap();
    let export = Uuid::new_v4();
    let request = json!({"project_revision": revision}).to_string();
    let mut tx = pool.begin().await.unwrap();
    sqlx::query("SET LOCAL session_replication_role=replica")
        .execute(&mut *tx)
        .await
        .unwrap();
    sqlx::query("INSERT INTO profiled_source_exports(operation_id,project_id,request_bytes,request_digest,reserved_bytes,charged_bytes,state) VALUES($1,$2,convert_to($3,'UTF8'),encode(sha256(convert_to($3,'UTF8')),'hex'),1048577,1048577,'pending')")
        .bind(export).bind(x.project()).bind(&request).execute(&mut *tx).await.unwrap();
    sqlx::query(
        "INSERT INTO profiled_launch_source_exports(attempt_id,operation_id) VALUES($1,$2)",
    )
    .bind(predecessor)
    .bind(export)
    .execute(&mut *tx)
    .await
    .unwrap();
    tx.commit().await.unwrap();
}

async fn assert_replacement_ready(x: &Fixture, pool: &sqlx::PgPool, task: Uuid, predecessor: Uuid) {
    use aq_server::profiled_execution::replacement::{self, Assessment};
    let released: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM profiled_replacement_releases WHERE attempt_id=$1 AND task_id=$2",
    )
    .bind(predecessor)
    .bind(task)
    .fetch_one(pool)
    .await
    .unwrap();
    assert_eq!(released, 1);
    let Assessment::Replace(next) =
        replacement::assess_task(&x.f.state, task, common::profiled::NOW)
            .await
            .unwrap()
    else {
        panic!("scheduler may start the replacement attempt");
    };
    assert_eq!(next.predecessor, predecessor);
    assert!(!next.needs_release);
}
