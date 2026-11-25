use super::*;

pub(super) type Log = Arc<Mutex<Vec<(String, Value)>>>;

/// Answers every Host8 command synchronously. `reviews` are the review
/// dispositions in order; `checks` the check outcomes in order (default pass).
pub(super) struct Peer {
    log: Log,
    reviews: Vec<&'static str>,
    /// Human decision outcomes in order (default `approved`).
    pub(super) decisions: Vec<&'static str>,
    registry: Registry,
    /// When set, work.ensure answers like the real Host7 adapter: `accepted`
    /// with a handle, its completion queued here as (handle, event payload).
    pub(super) async_work: Option<Completions>,
}
pub(super) type Completions = Arc<Mutex<Vec<(String, Value)>>>;
impl Peer {
    pub(super) fn new(log: Log, reviews: Vec<&'static str>) -> Self {
        Self {
            log,
            reviews,
            decisions: Vec::new(),
            registry: CatalogProfile::Host8.registry(),
            async_work: None,
        }
    }
    pub(super) async fn start(self, namespace: &str, store: MemoryStorage) -> Owner {
        let registration = aq_playbook_host::Registration::new(
            namespace.into(),
            aq_playbook_host::Limits::default(),
            self.registry.clone(),
        );
        Owner::start_registered(registration, move || Ok(store), move |_| Ok(self))
            .await
            .unwrap()
    }

    /// The host's completion for this work, from the production mapping of a
    /// terminal graph task to the aq.operation.updated payload.
    fn accept_work(&self, queue: &Completions, n: usize, input: &Value) -> DispatchResult {
        let mut node = json!({"version": 2, "task_state": "completed", "metadata": {}});
        if input["kind"] == "review" {
            node["metadata"]["aq:review_disposition"] = json!(
                self.reviews
                    .get(self.review_count() - 1)
                    .copied()
                    .unwrap_or("accepted")
            );
        }
        let task = format!("work-{n}");
        let request = json!({"kind": input["kind"], "reference": format!("aq.playbook.work/{task}"),
            "subject": {"id": task, "revision": 1}});
        let done = aq_server::playbooks::host_adapters::work_completion(&request, Some(&node))
            .unwrap()
            .unwrap();
        let handle = format!("operation-{n}");
        queue.lock().unwrap().push((
            handle.clone(),
            json!({"outcome": done.outcome, "output": done.output}),
        ));
        DispatchResult::Accepted { handle }
    }
    fn review_count(&self) -> usize {
        self.log
            .lock()
            .unwrap()
            .iter()
            .filter(|(c, v)| c == "work.ensure" && v["kind"] == "review")
            .count()
    }
    fn count(&self, name: &str) -> usize {
        self.log
            .lock()
            .unwrap()
            .iter()
            .filter(|(n, _)| n == name)
            .count()
    }
}
/// Catalog form of the n-th prepared candidate's OID (distinct from the
/// request's `source`, so a check bound to `input.source` is visible).
pub(super) fn candidate_source(n: usize) -> Value {
    json!({"format": "sha1", "hex": format!("{n:040x}")})
}
fn subject(id: String) -> Value {
    json!({"id": id, "revision": 1})
}
impl Host for Peer {
    fn authorize(&self, _: &Authority, c: &CommandContract) -> bool {
        self.registry.commands.get(&c.name) == Some(c)
    }
    fn dispatch(&mut self, intent: &Intent) -> DispatchResult {
        let name = intent.command.name.clone();
        let n = self.count(&name) + 1;
        let input = intent.input.clone();
        self.log.lock().unwrap().push((name.clone(), input.clone()));
        if let (Some(queue), "work.ensure") = (&self.async_work, name.as_str()) {
            return self.accept_work(queue, n, &input);
        }
        let (outcome, output) = match name.as_str() {
            "work.ensure" if input["kind"] == "review" => {
                let i = self.review_count();
                let disposition = self.reviews.get(i - 1).copied().unwrap_or("accepted");
                (
                    "completed",
                    json!({"disposition": disposition,
                           "reference": format!("aq.review.decision/r{i}"),
                           "subject": input["subject"]}),
                )
            }
            "work.ensure" => (
                "completed",
                json!({"disposition": "accepted", "reference": format!("work:{n}"),
                       "subject": subject(format!("change-{n}"))}),
            ),
            "candidate.prepare" => (
                "prepared",
                json!({"reference": format!("candidate:{n}"),
                       "source": candidate_source(n),
                       "subject": subject(format!("candidate-{n}"))}),
            ),
            "check.request" => check_receipt(&input, n),
            "decision.request" => (
                self.decisions.get(n - 1).copied().unwrap_or("approved"),
                json!({"reference": format!("aq.decision/d{n}"),
                       "subject": input["subject"]}),
            ),
            "candidate.publish" => (
                "published",
                json!({"reference": format!("publication:{n}"),
                       "subject": input["destination"]}),
            ),
            _ => {
                return DispatchResult::Unknown {
                    reason: format!("unexpected {name}"),
                };
            }
        };
        DispatchResult::Completed {
            outcome: outcome.into(),
            output,
        }
    }
    fn reconcile(&mut self, intent: &Intent) -> DispatchResult {
        DispatchResult::Unknown {
            reason: format!("no reconcile for {}", intent.command.name),
        }
    }
}
impl DomainHost for Peer {
    fn set_mode(&mut self, _: HostMode) {}
}

fn check_receipt(input: &Value, n: usize) -> (&'static str, Value) {
    (
        "passed",
        json!({
            "check": input["check"],
            "disposition": "passed",
            "receipt": {
                "digest": hash_bytes(format!("receipt-{n}").as_bytes()),
                "executor_version": "fake",
                "exit": {"code": 0, "stop": "exited"},
                "inputs_digest": hash_bytes(b"inputs"),
                "result_artifacts": [],
            },
            "reference": format!("check-receipt:{n}"),
            "source": input["source"],
            "subject": input["subject"],
        }),
    )
}
