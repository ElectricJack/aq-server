use super::*;

pub(super) enum Effect<'a> {
    Ensure,
    Change,
    Cancel(&'a str),
}
pub(super) enum PreparedEffect {
    Complete(HostResult),
    Pending(Value),
}

impl Service {
    pub(super) async fn prepare_effect(
        &self,
        owner: &RuntimeOwner,
        command: HostIntent<'_>,
        kind: Effect<'_>,
    ) -> Result<(PreparedEffect, String)> {
        let (mut tx, stage, identity) = self.begin(Some(owner), command).await?;
        let prepared = match stage.ok_or(Error::Unknown)? {
            Stage::Retained(result) => {
                tx.commit().await?;
                PreparedEffect::Complete(result)
            }
            Stage::Pending(plan) => {
                tx.commit().await?;
                PreparedEffect::Pending(plan)
            }
            Stage::Fresh => {
                let plan = match kind {
                    Effect::Ensure => self.ensure_plan(&mut tx, command.intent).await?,
                    Effect::Change => self.change_plan(&mut tx, command.intent).await?,
                    Effect::Cancel(handle) => {
                        self.cancel_plan(&mut tx, command.intent, handle).await?
                    }
                };
                match plan {
                    PreparedEffect::Complete(result) => PreparedEffect::Complete(
                        self.settle_fresh(tx, command.retained(&identity), result)
                            .await?,
                    ),
                    PreparedEffect::Pending(plan) => {
                        self.record(
                            &mut tx,
                            command.retained(&identity),
                            EffectReceipt::new(PENDING, &plan),
                        )
                        .await?;
                        self.commit_fresh(tx, command.grant, command.intent).await?;
                        PreparedEffect::Pending(plan)
                    }
                }
            }
        };
        Ok((prepared, identity))
    }
}
