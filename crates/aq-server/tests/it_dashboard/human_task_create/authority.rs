use super::*;
impl Scenario {
    pub(super) async fn check_native_lease(&self) {
        // A second explicit recovery observes the lease and cannot double-dispatch.
        // Revoking Owner while a real response is held prevents returning a stale-authority receipt.
        let racing = Uuid::new_v4();
        self.trace.hold.store(true, Ordering::SeqCst);
        let race_app = self.app.clone();
        let race_path = self.path.clone();
        let pending = tokio::spawn(async move {
            send(
                &race_app,
                &race_path,
                Some(&request(racing, "1")),
                "owner@localhost",
            )
            .await
        });
        self.trace.arrived.notified().await;
        let before = self.trace.count();
        let (code, leased) = self
            .owner(
                &format!("/api/human/operations/{racing}/recover"),
                Some("{}"),
            )
            .await;
        assert_eq!(code, StatusCode::ACCEPTED, "{leased}");
        assert_eq!(leased["recovery"]["reason"], "dispatch_in_progress");
        assert_eq!(self.trace.count(), before);
        tokio::time::timeout(
            std::time::Duration::from_secs(1),
            sqlx::query("UPDATE users SET role='viewer' WHERE id=$1")
                .bind(self.f.actor)
                .execute(&self.pool),
        )
        .await
        .unwrap()
        .unwrap();
        self.trace.release.notify_one();
        let (code, withheld) = pending.await.unwrap();
        assert_eq!(code, StatusCode::CONFLICT, "{withheld}");
        sqlx::query("UPDATE users SET role='owner' WHERE id=$1")
            .bind(self.f.actor)
            .execute(&self.pool)
            .await
            .unwrap();
        let (code, durable) = self
            .owner(&format!("/api/human/operations/{racing}"), None)
            .await;
        assert_eq!(code, StatusCode::OK, "{durable}");
        assert_eq!(durable["state"], "confirmed");
        assert_eq!(self.trace.count(), before);
        let (code, reconciled) = self
            .owner(&format!("/api/v1/operations/{racing}"), None)
            .await;
        assert_eq!(code, StatusCode::OK, "{reconciled}");
        assert_eq!(
            reconciled["data"]["disposition"], "confirmed",
            "a committed native task must remain reconcilable"
        );
    }
    pub(super) async fn check_union_revocation(&self) {
        // The union route must withhold admission metadata after owner revocation too.
        let union_race = Uuid::new_v4();
        self.trace.hold.store(true, Ordering::SeqCst);
        let race_app = self.app.clone();
        let union_body = self.action(union_race, "1");
        let pending = tokio::spawn(async move {
            send(
                &race_app,
                "/api/v1/actions",
                Some(&union_body),
                "owner@localhost",
            )
            .await
        });
        self.trace.arrived.notified().await;
        sqlx::query("UPDATE users SET role='viewer' WHERE id=$1")
            .bind(self.f.actor)
            .execute(&self.pool)
            .await
            .unwrap();
        self.trace.release.notify_one();
        let (code, withheld) = pending.await.unwrap();
        assert_eq!(code, StatusCode::FORBIDDEN, "{withheld}");
        assert!(withheld.get("data").is_none(), "{withheld}");
        assert!(withheld.get("pending_operations").is_none(), "{withheld}");
        sqlx::query("UPDATE users SET role='owner' WHERE id=$1")
            .bind(self.f.actor)
            .execute(&self.pool)
            .await
            .unwrap();
        let (code, reconciled) = self
            .owner(&format!("/api/v1/operations/{union_race}"), None)
            .await;
        assert_eq!(code, StatusCode::OK, "{reconciled}");
        assert_eq!(reconciled["data"]["disposition"], "confirmed");
    }
    pub(super) async fn check_recovery_fences(&self) -> (Uuid, usize) {
        let project = self.project; // Unknown remains possible under changed service or project; neither can retry.
        let blocked = Uuid::new_v4();
        self.trace.lose.store(true, Ordering::SeqCst);
        assert_eq!(
            self.owner(&self.path, Some(&request(blocked, "1"))).await.0,
            StatusCode::ACCEPTED
        );
        let before = self.trace.count();
        let blocked_path = format!("/api/human/operations/{blocked}");
        let mut changed = self.config.clone();
        changed.mutation_actor_id = Some("different-service-actor".into());
        let mut other = self.f.state.clone();
        other.agent_access = Some(Arc::new(Runtime::new(&changed).await.unwrap()));
        let other_app = human::routes(&other).with_state(other);
        let (_, v) = send(
            &other_app,
            &format!("{blocked_path}/recover"),
            Some("{}"),
            "owner@localhost",
        )
        .await;
        assert_eq!(v["state"], "unknown");
        assert_eq!(v["effect"], "possible");
        assert_eq!(v["recovery"]["reason"], "graph_service_changed");
        assert_eq!(self.trace.count(), before);
        sqlx::query("UPDATE project_execution_bindings SET revision=2 WHERE project_id=$1")
            .bind(project)
            .execute(&self.pool)
            .await
            .unwrap();
        let (_, v) = self
            .owner(&format!("{blocked_path}/recover"), Some("{}"))
            .await;
        assert_eq!(v["state"], "unknown");
        assert_eq!(v["effect"], "possible");
        assert_eq!(v["recovery"]["reason"], "project_binding_changed");
        assert_eq!(self.trace.count(), before);
        (blocked, before)
    }
    pub(super) async fn check_foreign_owner(
        &self,
        original: (Uuid, Value),
        lost: Uuid,
        before: usize,
    ) {
        let (op, created) = original;
        let lookup = format!("/api/human/operations/{op}");
        let project = self.project; // A confirmed receipt is original-workspace proof even after rebinding.
        assert_eq!(self.owner(&lookup, None).await.1, created);
        let foreign = Uuid::new_v4();
        sqlx::query("INSERT INTO users(id,email,role,issuer,subject,created_at) VALUES($1,'other@localhost','owner','dev','other@localhost',now())").bind(foreign).execute(&self.pool).await.unwrap();
        assert_eq!(
            send(&self.app, &lookup, None, "other@localhost").await.0,
            StatusCode::NOT_FOUND
        );
        sqlx::query("UPDATE users SET role='viewer' WHERE id=$1")
            .bind(self.f.actor)
            .execute(&self.pool)
            .await
            .unwrap();
        assert_eq!(self.owner(&lookup, None).await.0, StatusCode::FORBIDDEN);
        assert_eq!(
            send(
                &self.app,
                &self.path,
                Some(&request(Uuid::new_v4(), "2")),
                "owner@localhost"
            )
            .await
            .0,
            StatusCode::FORBIDDEN
        );
        assert_eq!(self.trace.count(), before);
        assert_eq!(
            self.owner(&format!("/api/v1/operations/{lost}"), None)
                .await
                .0,
            404
        );
        sqlx::query("UPDATE users SET role='owner' WHERE id=$1")
            .bind(self.f.actor)
            .execute(&self.pool)
            .await
            .unwrap();
        sqlx::query("UPDATE project_execution_bindings SET revision=1 WHERE project_id=$1")
            .bind(project)
            .execute(&self.pool)
            .await
            .unwrap();
    }
    pub(super) async fn check_union_replay(&self, before: usize) {
        let union_id = Uuid::new_v4();
        let (code, first) = self
            .owner("/api/v1/actions", Some(&self.action(union_id, "1")))
            .await;
        assert_eq!(code, 200, "{first}");
        assert_eq!(first["data"]["disposition"], "confirmed");
        let (code, native) = self.owner(&self.path, Some(&request(union_id, "1"))).await;
        assert_eq!(code, 200, "{native}");
        assert_eq!(native["task_id"], json!(union_id));
        assert_eq!(self.trace.count(), before + 1);
    }
    pub(super) async fn check_legacy_discovery(&self, blocked: Uuid) {
        // Model a retained operation admitted before the union journal existed.
        // Only the isolated test database's union identity trigger is disabled.
        sqlx::query("ALTER TABLE dashboard_operations DISABLE TRIGGER USER")
            .execute(&self.pool)
            .await
            .unwrap();
        sqlx::query("DELETE FROM dashboard_operations WHERE operation_id=$1")
            .bind(blocked)
            .execute(&self.pool)
            .await
            .unwrap();
        sqlx::query("ALTER TABLE dashboard_operations ENABLE TRIGGER USER")
            .execute(&self.pool)
            .await
            .unwrap();
        let count = self.trace.count();
        let bootstrap = self.owner("/api/v1/bootstrap", None).await.1;
        assert!(
            bootstrap["pending_operations"]
                .as_array()
                .unwrap()
                .contains(&json!(blocked)),
            "{bootstrap}"
        );
        assert_eq!(
            send(
                &self.app,
                &format!("/api/v1/operations/{blocked}"),
                None,
                "other@localhost"
            )
            .await
            .0,
            StatusCode::NOT_FOUND
        );
        let (code, historical) = self
            .owner(&format!("/api/v1/operations/{blocked}"), None)
            .await;
        assert_eq!(code, StatusCode::OK, "{historical}");
        assert_eq!(historical["data"]["disposition"], "unknown");
        assert_eq!(historical["data"]["operation_id"], json!(blocked));
        assert_eq!(
            self.trace.count(),
            count,
            "historical discovery must never redispatch"
        );
    }
}
