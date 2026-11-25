use super::*;
pub(super) struct Operation<'a> {
    pub actor: Uuid,
    pub id: Uuid,
    pub hash: &'a str,
}
impl Operation<'_> {
    pub(super) async fn replay(&self, tx: &mut Transaction<'_, Postgres>) -> Result<Option<Value>> {
        if self.id.is_nil() {
            return Err(Error::Invalid);
        }
        let row=sqlx::query("SELECT request_digest,response FROM notification_operations WHERE actor_id=$1 AND operation_id=$2").bind(self.actor).bind(self.id).fetch_optional(&mut **tx).await?;
        match row {
            None => Ok(None),
            Some(row) => {
                if row.get::<String, _>("request_digest") != self.hash {
                    return Err(Error::Conflict);
                }
                Ok(Some(row.get("response")))
            }
        }
    }
    pub(super) async fn record(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        response: &Value,
    ) -> Result<()> {
        if serde_json::to_vec(response)?.len() > 16384 {
            return Err(Error::Invalid);
        }
        sqlx::query("INSERT INTO notification_operations(actor_id,operation_id,request_digest,response) VALUES($1,$2,$3,$4)").bind(self.actor).bind(self.id).bind(self.hash).bind(response).execute(&mut **tx).await?;
        Ok(())
    }
}

pub(super) enum Reservation {
    Operation,
    Route { created: bool },
    Message { deliveries: usize },
}
impl Reservation {
    pub(super) async fn reserve(self, tx: &mut Transaction<'_, Postgres>) -> Result<()> {
        let (messages, deliveries, routes) = match self {
            Self::Operation => (0, 0, 0),
            Self::Route { created } => (0, 0, i64::from(created)),
            Self::Message { deliveries } => (1, deliveries as i64, 0),
        };
        let operations = 1_i64;
        let bytes = messages * 16384 + deliveries * 32768 + routes * 16384 + operations * 16384;
        let changed=sqlx::query("UPDATE notification_budget SET reserved_bytes=reserved_bytes+$1,message_count=message_count+$2,delivery_count=delivery_count+$3,route_count=route_count+$4,operation_count=operation_count+$5 WHERE singleton AND reserved_bytes+$1<=134217728 AND message_count+$2<=4096 AND delivery_count+$3<=4096 AND route_count+$4<=128 AND operation_count+$5<=8192")
        .bind(bytes).bind(messages).bind(deliveries).bind(routes).bind(operations).execute(&mut **tx).await?.rows_affected();
        if changed != 1 {
            return Err(Error::Overloaded);
        }
        Ok(())
    }
}
