//! Actual daemon listeners and durable execution control while the independent
//! playbook owner is held inside its synchronous backend initializer. This does
//! not register a playbook host in production or qualify native harness launch.
mod common;
use aq_playbook_host::{Limits, MemoryStorage, Owner};
use aq_server::{build_admin_router, build_worker_router};
use aq_worker_protocol::*;
use common::profiled::{NOW, fixture};
use serde_json::json;
use std::{
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
use uuid::Uuid;
#[tokio::test(flavor = "current_thread")]
async fn actual_control_renew_cancel_and_readiness_progress_during_playbook_storage_stall() {
    let f = fixture().await;
    let spec = ExecutionSpec {
        bindings: None,
        project_id: Uuid::new_v4(),
        harness: FAKE_HARNESS_CAPABILITY.into(),
        resolved_spec_digest: String::new(),
        fixture_output: "explicit synthetic execution control fixture".into(),
        resources: Resources {
            slots: 1,
            cpu_millis: 100,
            memory_bytes: 1024,
            disk_bytes: 1024,
        },
        required_capabilities: vec![FAKE_HARNESS_CAPABILITY.into()],
    }
    .seal();
    let offer = Offer {
        offer_id: Uuid::new_v4(),
        identity: f.node.clone(),
        attempt: AttemptRef {
            attempt_id: Uuid::new_v4(),
            session_id: Uuid::new_v4(),
            generation: 1,
        },
        spec,
        offer_expires_at_ms: NOW + 30_000,
        reservation_ttl_ms: 30_000,
    };
    f.state
        .store
        .execution_offer(Uuid::new_v4(), &offer)
        .await
        .unwrap();
    f.state
        .store
        .execution_accept(
            &AcceptRequest {
                identity: f.node.clone(),
                operation_id: Uuid::new_v4(),
                offer_id: offer.offer_id,
                attempt: offer.attempt.clone(),
                resolved_spec_digest: offer.spec.resolved_spec_digest.clone(),
                reservation_id: Uuid::new_v4(),
                reservation_deadline_ms: 500_000,
                reservation_remaining_ms: 20_000,
            },
            NOW,
        )
        .await
        .unwrap();
    let admin = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let worker = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let a = admin.local_addr().unwrap();
    let w = worker.local_addr().unwrap();
    let ar = build_admin_router(f.state.clone());
    let wr = build_worker_router(f.state.clone());
    let aserver = tokio::spawn(async move {
        axum::serve(
            admin,
            ar.into_make_service_with_connect_info::<std::net::SocketAddr>(),
        )
        .await
        .unwrap()
    });
    let wserver = tokio::spawn(async move {
        axum::serve(
            worker,
            wr.into_make_service_with_connect_info::<std::net::SocketAddr>(),
        )
        .await
        .unwrap()
    });
    let entered = Arc::new(AtomicBool::new(false));
    let gate = Arc::new((Mutex::new(false), Condvar::new()));
    let g = gate.clone();
    let e = entered.clone();
    let start = tokio::spawn(async move {
        Owner::start("isolated-http".into(), Limits::default(), move || {
            e.store(true, Ordering::Release);
            let (lock, cv) = &*g;
            let mut released = lock.lock().unwrap();
            while !*released {
                released = cv.wait(released).unwrap();
            }
            Ok(MemoryStorage::default())
        })
        .await
    });
    tokio::time::timeout(Duration::from_secs(2), async {
        while !entered.load(Ordering::Acquire) {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(2))
        .build()
        .unwrap();
    let outcome = async {
        for addr in [a, w] {
            assert_eq!(
                client
                    .get(format!("http://{addr}/readyz"))
                    .send()
                    .await
                    .unwrap()
                    .status(),
                200
            );
        }
        let renewed = client
            .post(format!("http://{w}/v2/worker/renew"))
            .bearer_auth(&f.credential)
            .json(&RenewRequest {
                identity: f.node.clone(),
                operation_id: Uuid::new_v4(),
                attempt: offer.attempt.clone(),
                grant_sequence: 1,
            })
            .send()
            .await
            .unwrap();
        let status = renewed.status();
        let body = renewed.text().await.unwrap();
        assert_eq!(status, 200, "{body}");
        let cancel=client.post(format!("http://{a}/api/execution/attempts/{}/cancel",offer.attempt.attempt_id)).header("x-aq-request","1").json(&json!({"operation_id":Uuid::new_v4(),"reason":"control survives stalled playbook backend"})).send().await.unwrap();
        let status = cancel.status();
        let body = cancel.text().await.unwrap();
        assert_eq!(status, 200, "{body}");
    };
    // Release even if assertion panics, so test failures cannot strand a thread.
    struct Release(Arc<(Mutex<bool>, Condvar)>);
    impl Drop for Release {
        fn drop(&mut self) {
            let (lock, cv) = &*self.0;
            *lock.lock().unwrap() = true;
            cv.notify_all();
        }
    }
    let release = Release(gate);
    outcome.await;
    drop(release);
    drop(start.await.unwrap().unwrap());
    aserver.abort();
    wserver.abort();
}
