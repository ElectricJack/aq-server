use aq_human_work::task_git_author::{TaskGitAuthor, lookup};
use aq_server::test_support::{TestDb, migrate_profile};
use serde_json::{Value, json};
use sqlx::PgPool;
use uuid::Uuid;

const DIGEST: &str = "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

struct Intent<'a> {
    principal: Uuid,
    project: Uuid,
    workspace: Uuid,
    operations: &'a [Value],
    affected: &'a [Uuid],
}

fn task(id: Uuid) -> Value {
    json!({"op":"create_node","kind":"task","id":id})
}

fn link(kind: &str, source: Uuid, target: Uuid) -> Value {
    json!({"op":"add_edge","edge_type":kind,"source":source,"target":target})
}

fn receipt(operation: Uuid, project: Uuid, disposition: &str, affected: &[Uuid]) -> Value {
    let entities: Vec<_> = affected
        .iter()
        .map(|id| json!({"type":"task","id":id}))
        .collect();
    json!({
        "operation_id":operation,
        "request_digest":DIGEST,
        "disposition":disposition,
        "scope":{"kind":"project","project_id":project},
        "receipt_refs":[{"type":"graph_change_set","id":operation}],
        "affected_entities":entities,
    })
}

impl Intent<'_> {
    async fn admit(&self, pool: &PgPool, disposition: &str) -> Uuid {
        let operation = Uuid::new_v4();
        let response = receipt(operation, self.project, disposition, self.affected);
        sqlx::query(
            "INSERT INTO dashboard_operations
                (operation_id,principal_id,kind,scope,request_digest,
                 disposition,response,created_at,updated_at)
             VALUES($1,$2,'task.graph.create',$3,$4,$5,$6,now(),now())",
        )
        .bind(operation)
        .bind(self.principal)
        .bind(&response["scope"])
        .bind(DIGEST)
        .bind(disposition)
        .bind(&response)
        .execute(pool)
        .await
        .unwrap();
        let body = serde_json::to_vec(&json!({"operations":self.operations})).unwrap();
        sqlx::query(
            "INSERT INTO dashboard_graph_intents
                (operation_id,project_id,project_revision,project_binding,workspace_id,
                 service_identity,idempotency_key,body)
             VALUES($1,$2,1,'{}',$3,'{}',$4,$5)",
        )
        .bind(operation)
        .bind(self.project)
        .bind(self.workspace)
        .bind(format!("aq-dashboard-action:{operation}"))
        .bind(body)
        .execute(pool)
        .await
        .unwrap();
        operation
    }

    async fn confirm(&self, pool: &PgPool, operation: Uuid) {
        sqlx::query(
            "UPDATE dashboard_operations SET disposition='confirmed', response=$2
             WHERE operation_id=$1",
        )
        .bind(operation)
        .bind(receipt(operation, self.project, "confirmed", self.affected))
        .execute(pool)
        .await
        .unwrap();
    }
}

async fn owner(pool: &PgPool, email: &str) -> Uuid {
    let id = Uuid::new_v4();
    sqlx::query("INSERT INTO users (id,email,role,created_at) VALUES($1,$2,'owner',now())")
        .bind(id)
        .bind(email)
        .execute(pool)
        .await
        .unwrap();
    id
}

async fn supervisor_session(pool: &PgPool) -> Uuid {
    let actor = Uuid::new_v4();
    let session = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO central_session_heads (actor_id,conversation_id,authority_id,current_generation)
         VALUES($1,$2,$3,1)",
    )
    .bind(actor)
    .bind(Uuid::new_v4())
    .bind(Uuid::new_v4())
    .execute(pool)
    .await
    .unwrap();
    let state = json!({
        "document": "state",
        "data": {"plan": {
            "subject": {"actor_id": actor},
            "generation": 1,
            "session": {"kind": "central", "session_id": session, "generation": 1},
        }},
    });
    sqlx::query(
        "INSERT INTO central_session_generations
            (actor_id,generation,session_id,revision,state_bytes,state_digest)
         VALUES($1,1,$2,1,$3,encode(sha256(convert_to($3,'UTF8')),'hex'))",
    )
    .bind(actor)
    .bind(session)
    .bind(state.to_string())
    .execute(pool)
    .await
    .unwrap();
    session
}

#[tokio::test]
async fn snapshots_only_confirmed_creations_and_inherits_the_sealed_parent() {
    let db = TestDb::create().await;
    let pool = db.pool().await;
    migrate_profile(&pool, 161).await.unwrap();
    let fixture = seed_legacy_authors(&pool).await;
    let other_workspace = assert_direct_child_inherits(&pool, &fixture).await;
    assert_service_descendants_inherit(&pool, &fixture).await;
    assert_workspace_reuse_is_independent(&pool, &fixture, other_workspace).await;
}

struct AuthorFixture {
    project: Uuid,
    workspace: Uuid,
    legacy: Uuid,
    requester: Uuid,
    snapshot: Option<TaskGitAuthor>,
}

async fn seed_legacy_authors(pool: &PgPool) -> AuthorFixture {
    let (project, workspace, legacy, unrelated) = (
        Uuid::new_v4(),
        Uuid::new_v4(),
        Uuid::new_v4(),
        Uuid::new_v4(),
    );
    let email = "First.Last+Git@Example.Test";
    let requester = owner(pool, email).await;

    Intent {
        principal: requester,
        project,
        workspace,
        operations: &[task(legacy), task(unrelated)],
        affected: &[legacy],
    }
    .admit(pool, "confirmed")
    .await;
    let malformed = Uuid::new_v4();
    let malformed_email = "Old<Account@@example.test";
    Intent {
        principal: owner(pool, malformed_email).await,
        project,
        workspace,
        operations: &[task(malformed)],
        affected: &[malformed],
    }
    .admit(pool, "confirmed")
    .await;
    migrate_profile(pool, aq_server::STATE_FORMAT_VERSION)
        .await
        .unwrap();
    let snapshot = Some(TaskGitAuthor {
        name: "First.Last+Git".into(),
        email: email.into(),
    });
    assert_eq!(
        lookup(pool, project, workspace, legacy).await.unwrap(),
        snapshot
    );
    assert_eq!(
        lookup(pool, project, workspace, unrelated).await.unwrap(),
        None
    );

    assert_eq!(
        lookup(pool, project, workspace, malformed).await.unwrap(),
        None
    );
    let retained: String = sqlx::query_scalar(
        "SELECT email FROM task_git_authors WHERE project_id=$1 AND workspace_id=$2
         AND task_id=$3 AND confirmed",
    )
    .bind(project)
    .bind(workspace)
    .bind(malformed)
    .fetch_one(pool)
    .await
    .unwrap();
    assert_eq!(retained, malformed_email);

    AuthorFixture {
        project,
        workspace,
        legacy,
        requester,
        snapshot,
    }
}

async fn assert_direct_child_inherits(pool: &PgPool, fixture: &AuthorFixture) -> Uuid {
    let AuthorFixture {
        project,
        workspace,
        legacy,
        requester,
        ref snapshot,
    } = *fixture;
    let snapshot = snapshot.clone();
    let other_workspace = Uuid::new_v4();
    let direct_child = Uuid::new_v4();
    let intent = Intent {
        principal: requester,
        project,
        workspace,
        operations: &[
            task(direct_child),
            link("parent-child", legacy, direct_child),
        ],
        affected: &[direct_child],
    };
    let operation = intent.admit(pool, "pending").await;
    assert_eq!(
        lookup(pool, project, workspace, direct_child)
            .await
            .unwrap(),
        None
    );

    sqlx::query("UPDATE users SET email='Later@Example.Test' WHERE id=$1")
        .bind(requester)
        .execute(pool)
        .await
        .unwrap();
    intent.confirm(pool, operation).await;
    assert_eq!(
        lookup(pool, project, workspace, direct_child)
            .await
            .unwrap(),
        snapshot
    );
    assert_eq!(
        lookup(pool, project, other_workspace, legacy)
            .await
            .unwrap(),
        None
    );
    assert_eq!(
        lookup(pool, Uuid::new_v4(), workspace, legacy)
            .await
            .unwrap(),
        None
    );

    other_workspace
}

async fn assert_service_descendants_inherit(pool: &PgPool, fixture: &AuthorFixture) {
    let AuthorFixture {
        project,
        workspace,
        legacy,
        requester,
        ref snapshot,
    } = *fixture;
    let snapshot = snapshot.clone();
    let (child, grandchild, related_only, unlinked) = (
        Uuid::new_v4(),
        Uuid::new_v4(),
        Uuid::new_v4(),
        Uuid::new_v4(),
    );
    let service = Intent {
        principal: supervisor_session(pool).await,
        project,
        workspace,
        operations: &[
            task(child),
            task(grandchild),
            task(related_only),
            json!({"op":"create_node","kind":"task","id":unlinked,
                   "metadata":{"aq:human_actor_id":requester}}),
            link("parent-child", legacy, child),
            link("parent-child", child, grandchild),
            link("motivated_by", related_only, legacy),
        ],
        affected: &[child, grandchild, related_only, unlinked],
    };
    let operation = service.admit(pool, "pending").await;
    assert_eq!(
        lookup(pool, project, workspace, grandchild).await.unwrap(),
        None
    );
    service.confirm(pool, operation).await;
    assert_eq!(
        lookup(pool, project, workspace, child).await.unwrap(),
        snapshot
    );
    assert_eq!(
        lookup(pool, project, workspace, grandchild).await.unwrap(),
        snapshot
    );
    assert_eq!(
        lookup(pool, project, workspace, related_only)
            .await
            .unwrap(),
        None
    );
    assert_eq!(
        lookup(pool, project, workspace, unlinked).await.unwrap(),
        None
    );
}

async fn assert_workspace_reuse_is_independent(
    pool: &PgPool,
    fixture: &AuthorFixture,
    other_workspace: Uuid,
) {
    let AuthorFixture {
        project,
        workspace,
        legacy,
        requester,
        ref snapshot,
    } = *fixture;
    let snapshot = snapshot.clone();
    let reused = Intent {
        principal: requester,
        project,
        workspace: other_workspace,
        operations: &[task(legacy)],
        affected: &[legacy],
    };
    reused
        .confirm(pool, reused.admit(pool, "pending").await)
        .await;
    let mut tx = pool.begin().await.unwrap();
    let new = lookup(&mut *tx, project, other_workspace, legacy)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(new.email, "Later@Example.Test");
    assert_eq!(new.name, "Later");
    tx.rollback().await.unwrap();
    assert_eq!(
        lookup(pool, project, workspace, legacy).await.unwrap(),
        snapshot
    );
}
