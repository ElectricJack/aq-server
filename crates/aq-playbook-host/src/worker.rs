use super::*;

pub(super) fn open<S: Storage>(
    shared: &Shared,
    namespace: String,
    store: S,
) -> Result<Engine<S>, Error> {
    let mut engine =
        Engine::new(store, namespace, uuid::Uuid::new_v4().to_string()).map_err(engine_error)?;
    engine.config = shared.limits.engine.clone();
    let state = engine.snapshot().map_err(engine_error)?;
    if state
        .artifacts
        .values()
        .any(|a| !accepts_base(&shared.registry, &a.content.registry))
    {
        let _ = engine.release();
        return Err(Error::Invalid);
    }
    let mut stale_artifacts = BTreeSet::new();
    for (digest, artifact) in &state.artifacts {
        remember_contracts(shared, &artifact.content.registry);
        if !plugins_match(&shared.registry, &artifact.content.registry) {
            stale_artifacts.insert(digest.clone());
        }
    }
    let _ = shared.stale_artifacts.set(stale_artifacts);
    if let Some(policy) = shared
        .limits
        .engine_retention
        .changed(&state.retention.policy)
        && let Err(error) = engine.set_retention_policy(policy)
    {
        let _ = engine.release();
        return Err(engine_error(error));
    }
    Ok(engine)
}

pub(super) fn host<S: Storage, H: DomainHost>(
    engine: &Engine<S>,
    shared: &Shared,
    host_factory: impl FnOnce(AcquiredOwner) -> Result<H, Error>,
) -> Result<H, Error> {
    // Probe only interface support at startup, through the actual
    // acquired Engine. Never retain the returned current metadata.
    match engine.inspect_catalog(&runner::CatalogQuery::first(
        runner::CatalogKind::Definitions,
        1,
    )) {
        Ok(_) => {
            shared.catalog_supported.store(true, Ordering::Release);
        }
        Err(runner::CatalogError::Unsupported) => {}
        Err(error) => {
            return Err(engine_error(error));
        }
    }
    host_factory(AcquiredOwner {
        namespace: engine.namespace.clone(),
        incarnation: engine.owner.clone(),
    })
}

impl Shared {
    fn next(&self) -> Option<Envelope> {
        let mut q = self.queues.lock().unwrap_or_else(|e| e.into_inner());
        loop {
            if q.closing {
                q.work.clear();
                q.control.clear();
                break None;
            }
            if let Some(e) = q.control.pop_front().or_else(|| q.work.pop_front()) {
                break Some(e);
            }
            q = self.wake.wait(q).unwrap_or_else(|e| e.into_inner());
        }
    }
    pub(super) fn serve<S: Storage>(&self, engine: &Engine<S>, host: &mut impl DomainHost) {
        while let Some(envelope) = self.next() {
            if self.handle(engine, host, envelope) {
                break;
            }
        }
        {
            let mut q = self.queues.lock().unwrap_or_else(|e| e.into_inner());
            q.closing = true;
            q.work.clear();
            q.control.clear();
        }
    }
    fn handle<S: Storage>(
        &self,
        engine: &Engine<S>,
        host: &mut impl DomainHost,
        envelope: Envelope,
    ) -> bool {
        let stop = matches!(
            &envelope,
            Envelope::Operation {
                request: Request::Stop,
                ..
            }
        );
        if stop {
            self.catalog_supported.store(false, Ordering::Release);
        }
        match envelope {
            Envelope::Operation { request, reply } => {
                let result = if stop {
                    engine.release().map(|_| Value::Null).map_err(engine_error)
                } else {
                    operations::execute(engine, host, request, self)
                };
                // A size failure after a commit is unknown, never 'not applied'.
                let result = result.and_then(|v| {
                    if bounded_json(&v, self.limits.response_bytes) {
                        Ok(v)
                    } else {
                        Err(Error::Unknown)
                    }
                });
                let _ = reply.send(result);
            }
            Envelope::Catalog { query, reply } => {
                let result = engine
                    .inspect_catalog(&query)
                    .map_err(CatalogCallError::Catalog)
                    .and_then(|page| {
                        if serde_json::to_vec(&page)
                            .is_ok_and(|bytes| bytes.len() <= self.limits.response_bytes)
                        {
                            Ok(page)
                        } else {
                            Err(Error::Unknown.into())
                        }
                    });
                let _ = reply.send(result);
            }
        }
        stop
    }
}
