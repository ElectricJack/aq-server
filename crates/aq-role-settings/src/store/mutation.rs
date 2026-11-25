use super::*;
/// The most bytes of the supervisor document. The supervisor host installs
/// each turn, with this text in it, as a file of at most 64 KiB.
const SUPERVISOR_DOCUMENT_BYTES: usize = 16_384;
/// Apply one change (see the crate documentation). `Error::Problems` when the
/// resulting set is not valid, `Error::Conflict` when `expected_revision` is
/// stale or the operation id was used for another request.
pub async fn apply(
    pool: &PgPool,
    actor: Uuid,
    mutation: &Mutation,
    now_ms: i64,
) -> Result<Receipt> {
    apply_referenced(Writer::new(pool, actor, now_ms), mutation, None).await
}
/// [`apply`], with the references from outside this store that a delete must
/// respect. They are read in the change's transaction, after its lock, so a
/// reference saved under [`check_binding`] is always seen.
pub async fn apply_referenced(
    writer: Writer<'_>,
    mutation: &Mutation,
    references: Option<References>,
) -> Result<Receipt> {
    let operation = Operation::new(writer, mutation)?;
    let mut tx = writer.pool.begin().await?;
    let state = read_settings(&mut tx, Lock::Update).await?;
    if let Some(receipt) = operation.replay(&mut tx).await? {
        return Ok(receipt);
    }
    let mut change = Candidate {
        version: Version {
            revision: state.revision + 1,
            now_ms: writer.now_ms,
        },
        documents: documents(&mut tx, None).await?,
        state,
        problems: Vec::new(),
    };
    let current = change.stage(&mutation.change, &mut tx).await?;
    if current != mutation.expected_revision {
        return Err(conflict(
            "This changed since it was read; reload before saving",
        ));
    }
    change
        .check_references(&mutation.change, references, &mut tx)
        .await?;
    let snapshot = change.snapshot(&mutation.change)?;
    persist(&mut tx, &mutation.change, &change).await?;
    let receipt = change.activate(&mut tx, snapshot).await?;
    operation
        .record(&mut tx, &receipt, change.state.limits.operations_kept)
        .await?;
    tx.commit().await?;
    Ok(receipt)
}

struct Operation<'a> {
    writer: Writer<'a>,
    mutation: &'a Mutation,
    digest: String,
}
impl<'a> Operation<'a> {
    fn new(writer: Writer<'a>, mutation: &'a Mutation) -> Result<Self> {
        if mutation.operation_id.is_nil() {
            return Err(invalid("Enter a valid operation ID"));
        }
        if mutation.expected_revision < 0 {
            return Err(invalid("Invalid revision"));
        }
        let request_digest = aq_profiles::digest(&serde_json::json!({
            "change": mutation.change,
            "expected_revision": mutation.expected_revision,
        }))
        .map_err(|_| Error::Internal)?;
        Ok(Self {
            writer,
            mutation,
            digest: request_digest,
        })
    }
    async fn replay(&self, conn: &mut PgConnection) -> Result<Option<Receipt>> {
        let replayed = sqlx::query(
        "SELECT actor_id,request_digest,receipt FROM role_settings_operations WHERE operation_id=$1",
    )
    .bind(self.mutation.operation_id)
    .fetch_optional(conn)
    .await?;
        if let Some(row) = replayed {
            if row.get::<Uuid, _>("actor_id") != self.writer.actor
                || row.get::<String, _>("request_digest") != self.digest
            {
                return Err(conflict("Operation ID already used for another request"));
            }
            return serde_json::from_value(row.get("receipt"))
                .map(Some)
                .map_err(|_| Error::Internal);
        }
        Ok(None)
    }
    async fn record(&self, conn: &mut PgConnection, receipt: &Receipt, kept: u32) -> Result<()> {
        sqlx::query(
        "INSERT INTO role_settings_operations(operation_id,actor_id,request_digest,receipt,revision,created_at_ms) VALUES($1,$2,$3,$4,$5,$6)",
    )
    .bind(self.mutation.operation_id)
    .bind(self.writer.actor)
    .bind(&self.digest)
    .bind(serde_json::to_value(receipt).map_err(|_| Error::Internal)?)
    .bind(receipt.revision)
    .bind(self.writer.now_ms)
    .execute(&mut *conn)
    .await?;
        // Receipts are a rolling set too: a repeat is recognised while its id is
        // among the most recent `operations_kept`; later it is a new request.
        sqlx::query(
            "DELETE FROM role_settings_operations WHERE operation_id NOT IN
         (SELECT operation_id FROM role_settings_operations ORDER BY revision DESC LIMIT $1)",
        )
        .bind(i64::from(kept))
        .execute(&mut *conn)
        .await?;
        Ok(())
    }
}

struct Candidate {
    state: Settings,
    documents: Vec<Document>,
    problems: Vec<Problem>,
    version: Version,
}
impl Candidate {
    async fn stage(&mut self, change: &Change, conn: &mut PgConnection) -> Result<i64> {
        Ok(match change {
            Change::Save { kind, id, markdown } => self.save(*kind, id, markdown),
            Change::Delete { kind, id } => self.delete(*kind, id)?,
            Change::SelectionDefaults(defaults) => {
                self.state.selection_defaults = defaults.clone();
                self.state.selection_defaults_revision
            }
            Change::Limits(limits) => {
                self.state.limits = *limits;
                self.state.limits_revision
            }
            Change::RoutingDocument(_) => self.state.routing_revision,
            Change::ObservationLimits(_) => observation_limits(conn).await?.1,
            Change::SupervisorDocument(_) => supervisor_document(conn).await?.1,
            #[cfg(feature = "delivery_playbook")]
            Change::DeliveryDocument(_) => self.state.delivery_revision,
            #[cfg(feature = "delivery_playbook")]
            Change::DeliveryLimits(limits) => {
                self.state.delivery_limits = limits.resolve(self.state.delivery_limits);
                self.state.delivery_limits_revision
            }
            #[cfg(feature = "project_environments")]
            Change::EnvironmentLimits(_) => environment_limits_on(conn).await?.1,
        })
    }
    fn position(&self, kind: Kind, id: &str) -> Option<usize> {
        self.documents
            .iter()
            .position(|document| document.kind == kind && document.id == id)
    }
    fn save(&mut self, kind: Kind, id: &str, markdown: &str) -> i64 {
        if let Some(stated) = stated_id(kind, markdown).filter(|stated| stated != id) {
            self.problems.push(Problem {
                document: format!("{} document `{id}`", kind.label()),
                field: "id".into(),
                message: format!("is `{stated}`; a document saved as `{id}` must have that id"),
            });
        }
        let document = Document {
            kind,
            id: id.into(),
            markdown: markdown.into(),
            revision: self.version.revision,
            updated_at_ms: self.version.now_ms,
        };
        match self.position(kind, id) {
            Some(at) => std::mem::replace(&mut self.documents[at], document).revision,
            None => {
                self.documents.push(document);
                0
            }
        }
    }
    fn delete(&mut self, kind: Kind, id: &str) -> Result<i64> {
        let at = self
            .position(kind, id)
            .ok_or_else(|| Error::NotFound(format!("No {} `{id}` exists", kind.label())))?;
        Ok(self.documents.remove(at).revision)
    }
    async fn check_references(
        &mut self,
        change: &Change,
        references: Option<References>,
        conn: &mut PgConnection,
    ) -> Result<()> {
        if let (Change::Delete { kind, id }, Some(read)) = (change, references) {
            for reference in read(conn).await? {
                if reference.kind == *kind && reference.id == *id {
                    self.problems.push(Problem {
                        document: format!("{} `{id}`", kind.label()),
                        field: String::new(),
                        message: format!("is {}", reference.by),
                    });
                }
            }
        }
        Ok(())
    }
    fn snapshot(&mut self, change: &Change) -> Result<Option<Snapshot>> {
        match change {
            Change::RoutingDocument(markdown) => self.validate_markdown("routing", markdown)?,
            Change::ObservationLimits(limits) => {
                limits.validate().map_err(|message| invalid(&message))?
            }
            Change::SupervisorDocument(markdown) => {
                self.validate_markdown("supervisor", markdown)?;
                // It goes into every installed supervisor turn, which is bounded.
                if markdown.len() > SUPERVISOR_DOCUMENT_BYTES {
                    return Err(invalid(&format!(
                        "The supervisor document is {} bytes, over the limit of {SUPERVISOR_DOCUMENT_BYTES} bytes",
                        markdown.len()
                    )));
                }
            }
            #[cfg(feature = "delivery_playbook")]
            Change::DeliveryDocument(markdown) => self.validate_markdown("delivery", markdown)?,
            #[cfg(feature = "delivery_playbook")]
            Change::DeliveryLimits(_) => self
                .state
                .delivery_limits
                .validate()
                .map_err(|message| invalid(&message))?,
            // The environment limits, too, compile and activate nothing.
            #[cfg(feature = "project_environments")]
            Change::EnvironmentLimits(limits) => {
                limits.validate().map_err(|message| invalid(&message))?
            }
            _ => {
                let mut problems = std::mem::take(&mut self.problems);
                match compile(
                    &self.documents,
                    &self.state.selection_defaults,
                    &self.state.limits,
                ) {
                    Ok(snapshot) if problems.is_empty() => return Ok(Some(snapshot)),
                    Err(more) => problems.extend(more),
                    Ok(_) => (),
                }
                return Err(Error::Problems(problems));
            }
        }
        Ok(None)
    }
    fn validate_markdown(&self, kind: &str, markdown: &str) -> Result<()> {
        let limit = self.state.limits.documents.max_document_bytes;
        if markdown.len() > limit {
            return Err(invalid(&format!(
                "The {kind} document is {} bytes, over the limit of {limit} bytes",
                markdown.len()
            )));
        }
        Ok(())
    }
    async fn activate(
        &self,
        conn: &mut PgConnection,
        snapshot: Option<Snapshot>,
    ) -> Result<Receipt> {
        let digest = match snapshot {
            Some(snapshot) => {
                activate(conn, &snapshot, self.version, &self.state.limits).await?;
                snapshot.digest().to_owned()
            }
            None => self.state.active_digest.clone().unwrap_or_default(),
        };
        Ok(Receipt {
            revision: self.version.revision,
            digest,
        })
    }
}
async fn persist(conn: &mut PgConnection, change: &Change, candidate: &Candidate) -> Result<()> {
    let Version { revision, now_ms } = candidate.version;
    match change {
        Change::Save { kind, id, markdown } => {
            sqlx::query(
                "INSERT INTO role_documents(kind,id,markdown,revision,updated_at_ms) VALUES($1,$2,$3,$4,$5)
                 ON CONFLICT(kind,id) DO UPDATE SET markdown=EXCLUDED.markdown,revision=EXCLUDED.revision,updated_at_ms=EXCLUDED.updated_at_ms",
            )
            .bind(kind_name(*kind))
            .bind(id)
            .bind(markdown)
            .bind(revision)
            .bind(now_ms)
            .execute(&mut *conn)
            .await?;
        }
        Change::Delete { kind, id } => {
            sqlx::query("DELETE FROM role_documents WHERE kind=$1 AND id=$2")
                .bind(kind_name(*kind))
                .bind(id)
                .execute(&mut *conn)
                .await?;
        }
        Change::SelectionDefaults(defaults) => update_json(conn, "UPDATE role_settings SET selection_defaults=$2,selection_defaults_revision=$1 WHERE singleton",revision,defaults).await?,
        Change::Limits(limits) => update_json(conn, "UPDATE role_settings SET limits=$2,limits_revision=$1 WHERE singleton",revision,limits).await?,
        Change::RoutingDocument(markdown) => update_markdown(conn, "UPDATE role_settings SET revision=$1,routing_markdown=$2,routing_revision=$1 WHERE singleton", revision,markdown).await?,
        Change::ObservationLimits(limits) => update_json(conn, "UPDATE role_settings SET revision=$1,observation_limits=$2,observation_limits_revision=$1 WHERE singleton",revision,limits).await?,
        Change::SupervisorDocument(markdown) => update_markdown(conn, "UPDATE role_settings SET revision=$1,supervisor_markdown=$2,supervisor_revision=$1 WHERE singleton", revision,markdown).await?,
        #[cfg(feature = "delivery_playbook")]
        Change::DeliveryDocument(markdown) => update_markdown(conn, "UPDATE role_settings SET revision=$1,delivery_markdown=$2,delivery_revision=$1 WHERE singleton", revision,markdown).await?,
        #[cfg(feature = "delivery_playbook")]
        Change::DeliveryLimits(_) => update_json(conn, "UPDATE role_settings SET revision=$1,delivery_limits=$2,delivery_limits_revision=$1 WHERE singleton",revision,&candidate.state.delivery_limits).await?,
        #[cfg(feature = "project_environments")]
        Change::EnvironmentLimits(limits) => update_json(conn, "UPDATE role_settings SET revision=$1,environment_limits=$2,environment_limits_revision=$1 WHERE singleton",revision,limits).await?,
    }
    Ok(())
}
async fn update_json(
    conn: &mut PgConnection,
    sql: &'static str,
    revision: i64,
    value: &impl serde::Serialize,
) -> Result<()> {
    let value = serde_json::to_value(value).map_err(|_| Error::Internal)?;
    sqlx::query(sql)
        .bind(revision)
        .bind(value)
        .execute(conn)
        .await?;
    Ok(())
}
async fn update_markdown(
    conn: &mut PgConnection,
    sql: &'static str,
    revision: i64,
    markdown: &str,
) -> Result<()> {
    sqlx::query(sql)
        .bind(revision)
        .bind(markdown)
        .execute(conn)
        .await?;
    Ok(())
}
