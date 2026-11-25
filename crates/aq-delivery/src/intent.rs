use super::*;

/// Caller inputs and operation identity used to freeze and replay one wire request.
pub(super) struct IntentRequest<'a> {
    pub(super) context: &'a OperationContext,
    pub(super) name: &'a str,
    inputs: Value,
}
impl<'a> IntentRequest<'a> {
    pub(super) fn new<T: Serialize>(
        context: &'a OperationContext,
        name: &'a str,
        inputs: &T,
    ) -> Result<Self> {
        Ok(Self {
            context,
            name,
            inputs: serde_json::to_value(inputs)?,
        })
    }
    pub(super) fn decode<T: WireResult>(&self, raw: Vec<u8>, params: &Value) -> Result<Receipt<T>> {
        decode(raw, self.context, self.name, params)
    }
    // A transaction-scoped operation lock serializes freezing the original
    // payload, including when the row does not yet exist. Hash collisions only
    // serialize unrelated work; they never equate operation identities.
    pub(super) async fn prepare<'pool>(
        &self,
        pool: &'pool PgPool,
    ) -> Result<(Transaction<'pool, Postgres>, Option<Intent>)> {
        let (context, operation, inputs) = (self.context, self.name, &self.inputs);
        context.validate()?;
        let mut tx = pool.begin().await?;
        sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, 45))")
            .bind(context.operation_id.to_string())
            .execute(&mut *tx)
            .await?;
        let old = sqlx::query("SELECT input_digest,inputs,operation,request FROM delivery_intents WHERE operation_id=$1")
            .bind(context.operation_id)
            .fetch_optional(&mut *tx)
            .await?;
        let old = if let Some(row) = old {
            let same_inputs = row.get::<String, _>("input_digest") == digest(inputs)?
                && row.get::<Value, _>("inputs") == *inputs;
            if !same_inputs || row.get::<String, _>("operation") != operation {
                return Err(DeliveryError::IdempotencyConflict);
            }
            let request: Vec<u8> = row.get("request");
            let wire: Value = serde_json::from_slice(&request)?;
            Some(Intent {
                request,
                params: wire["params"].clone(),
            })
        } else {
            None
        };
        Ok((tx, old))
    }
    pub(super) async fn freeze(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        params: Value,
    ) -> Result<Intent> {
        let (context, operation, inputs) = (self.context, self.name, &self.inputs);
        let payload = json!({
            "operation": operation,
            "repository_id": context.repository_id,
            "expected_binding_generation": context.binding_generation,
            "params": params,
        });
        let request = json!({
            "protocol_version": 1,
            "request_id": context.operation_id.to_string(),
            "operation_id": context.operation_id,
            "operation": operation,
            "repository_id": context.repository_id,
            "expected_binding_generation": context.binding_generation,
            "params": params,
            "deadline_ms": 30000,
        });
        let mut bytes = serde_json::to_vec(&canonical(&request))?;
        bytes.push(b'\n');
        if bytes.len() > protocol::MAX_REQUEST {
            return Err(DeliveryError::InvalidInput("request exceeds 1 MiB"));
        }
        sqlx::query("INSERT INTO delivery_intents(operation_id,repository_id,binding_generation,operation,inputs,input_digest,payload_digest,request) VALUES($1,$2,$3,$4,$5,$6,$7,$8)")
            .bind(context.operation_id)
            .bind(context.repository_id)
            .bind(context.binding_generation as i64)
            .bind(operation)
            .bind(inputs)
            .bind(digest(inputs)?)
            .bind(digest(&payload)?)
            .bind(&bytes)
            .execute(&mut **tx)
            .await?;
        Ok(Intent {
            request: bytes,
            params,
        })
    }
}
