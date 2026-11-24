use super::*;
use aq_human_work::subtree_activation;

impl Service {
    pub(super) async fn work_activate(
        &self,
        owner: &RuntimeOwner,
        command: HostIntent<'_>,
    ) -> Result<HostResult> {
        let id = command.grant;
        let intent = command.intent;
        let ports = self.ports()?;
        let workspace = self.namespace.workspace_id;
        let (mut tx, stage, identity) = self.begin(Some(owner), command).await?;
        let (body, fresh) = match stage.ok_or(Error::Unknown)? {
            Stage::Retained(result) => {
                tx.commit().await?;
                return Ok(result);
            }
            Stage::Pending(body) => {
                tx.commit().await?;
                (body, false)
            }
            Stage::Fresh => {
                let subject = &intent.input["subject"];
                let mut plan = Err("not_ensured");
                if let Ok(task) = Uuid::parse_str(text(subject, "id")?)
                    && self.namespace_work(&mut tx, task).await?
                {
                    let expected = subject["revision"].as_u64().ok_or(Error::Invalid)?;
                    let tree = ports.graph.activation_tree(workspace, task).await?;
                    plan = subtree_activation::operations(&tree, workspace, task, expected)
                        .map_err(|error| error.reason());
                }
                let operations = match plan {
                    Ok(operations) => operations,
                    Err(reason) => {
                        return self
                            .settle_fresh(tx, command.retained(&identity), refused(reason))
                            .await;
                    }
                };
                let body = json!({
                    "idempotency_key": format!("aq-playbook-activate-{}", self.operation(intent)?),
                    "reason": null,
                    "operations": operations,
                });
                self.record(
                    &mut tx,
                    command.retained(&identity),
                    EffectReceipt::new(PENDING, &body),
                )
                .await?;
                self.commit_fresh(tx, id, intent).await?;
                (body, true)
            }
        };
        let (status, raw) = ports.graph.mutate(workspace, body.clone()).await?;
        let result = activation_receipt(status, &raw, &body, fresh)?;
        self.finish(intent, &identity, result).await
    }
}

fn refused(reason: &str) -> HostResult {
    host_result("refused", json!({"reason":reason}))
}
fn activation_receipt(status: u16, raw: &Value, body: &Value, fresh: bool) -> Result<HostResult> {
    Ok(match status {
        200..=299 => {
            let ops = body["operations"].as_array().ok_or(Error::Unknown)?;
            let receipt = subtree_activation::receipt(ops, raw).ok_or(Error::Unknown)?;
            HostResult {
                outcome: "activated".into(),
                output: json!({"activated": receipt.activated.len(), "skipped": receipt.skipped}),
            }
        }
        409 if raw["error"]["code"] == "conflict" => refused("subtree_conflict"),
        400 | 401 | 403 | 404 | 422 if fresh => refused("activation_refused"),
        _ => return Err(Error::Unknown),
    })
}
