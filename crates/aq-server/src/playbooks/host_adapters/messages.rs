//! D11 Host7 `message.send` (catalog 2.0.0 contract 1.0.0).
//!
//! A report is one bounded human notification to the namespace owner,
//! submitted through the existing notifications owner, which owns routing,
//! delivery and transport retry. The intent row (`pending`, carrying the
//! frozen `SendRequest`) commits before enqueue; the final `queued` row
//! commits after it. A lost or failed enqueue leaves the row pending and a
//! resumed dispatch re-submits the same fixed operation and message IDs, so
//! the notifications owner never stores a second message.
//!
//! The message id is derived from `identity` alone: a second intent naming
//! the same identity with the same body observes the first message; with a
//! different body it is refused (`identity_conflict`). One report identity is
//! therefore sent exactly once, whatever the run does.
use super::*;
use aq_notifications::SendRequest;

const MESSAGE_REF: &str = "aq.notification/";
/// Reports are retained for delivery for one day.
const MESSAGE_TTL_MS: u64 = 86_400_000;
const USER: &str = "user:";

fn bounded(value: &str) -> bool {
    !value.trim().is_empty() && value.len() <= 512 && !value.chars().any(char::is_control)
}

impl Service {
    pub(super) async fn host_message_send(
        &self,
        owner: &RuntimeOwner,
        id: Uuid,
        revision: i64,
        intent: &Intent,
    ) -> Result<HostResult> {
        let ports = self.ports()?;
        let (mut tx, stage, identity) = self.begin(Some(owner), id, revision, intent).await?;
        let input = &intent.input;
        let key = text(input, "identity")?;
        let message = stable_id(
            "aq.playbook.message",
            self.namespace.namespace_id,
            self.namespace.generation,
            key,
        )?;
        let output =
            json!({"reference": format!("{MESSAGE_REF}{message}"), "subject": subject(message, 1)});
        let plan = match stage.ok_or(Error::Unknown)? {
            Stage::Retained(result) => {
                tx.commit().await?;
                return Ok(result);
            }
            Stage::Pending(plan) => {
                tx.commit().await?;
                plan
            }
            Stage::Fresh => {
                let body = text(input, "body")?;
                let recipient = text(input, "recipient")?;
                // Only the namespace owner is a granted recipient: Host7
                // grants carry no recipient list, and no route is chosen here.
                let owner_user = format!("{USER}{}", self.namespace.owner_user_id);
                let refused = !bounded(key)
                    || recipient != owner_user
                    || text(input, "visibility")? != "project"
                    || body.trim().is_empty()
                    || body.encode_utf16().count() > 1600
                    || body
                        .chars()
                        .any(|c| c.is_control() && c != '\n' && c != '\t');
                if refused {
                    let result = HostResult {
                        outcome: "refused".into(),
                        output,
                    };
                    return self
                        .settle_fresh(tx, id, revision, intent, &identity, result)
                        .await;
                }
                let request = SendRequest {
                    operation_id: self.operation(intent)?,
                    message_id: message,
                    revision: 1,
                    scope: aq_lifecycle_api::Scope {
                        project_id: self.namespace.project_id,
                        workspace_id: self.namespace.workspace_id,
                    },
                    recipient_user_id: self.namespace.owner_user_id,
                    summary: body.to_string(),
                    ttl_ms: MESSAGE_TTL_MS,
                };
                let plan = json!({"request": request});
                self.record(&mut tx, id, revision, intent, &identity, PENDING, &plan)
                    .await?;
                self.commit_fresh(tx, id, intent).await?;
                plan
            }
        };
        let request: SendRequest = serde_json::from_value(plan["request"].clone())?;
        let outcome = match crate::notifications::api::service(&ports.state)
            .enqueue(self.namespace.owner_user_id, request)
            .await
        {
            Ok(_) => "queued",
            // The same identity already names a different message.
            Err(crate::notifications::Error::Conflict) => "refused",
            Err(error) => {
                // Pending: a resumed dispatch re-submits the same IDs.
                tracing::warn!(%error, "playbook message remains pending");
                return Err(Error::Unknown);
            }
        };
        self.finish(
            intent,
            &identity,
            HostResult {
                outcome: outcome.into(),
                output,
            },
        )
        .await
    }
}
