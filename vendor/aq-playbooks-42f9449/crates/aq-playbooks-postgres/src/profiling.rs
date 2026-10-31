//! Opt-in, bounded, value-free storage diagnostics. Counts SQL driver API calls,
//! not network packets: a string statement may require prepare plus execution.
use postgres::{Row, ToStatement, Transaction, types::ToSql};
use serde::Serialize;
use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
    time::Instant,
};
#[derive(Debug, Clone, Default, Serialize)]
pub struct StageProfile {
    pub calls: u64,
    pub elapsed_us: u64,
    pub sql_calls: u64,
    pub sql_wait_us: u64,
}
#[derive(Debug, Clone, Default, Serialize)]
pub struct OperationProfile {
    pub completed: u64,
    pub aborted: u64,
    pub stages: BTreeMap<String, StageProfile>,
    pub artifact_verifications: u64,
    pub artifact_verification_us: u64,
    pub artifact_canonicalizations: u64,
    pub artifact_canonicalization_us: u64,
    pub unchanged_artifacts_skipped: u64,
}
#[derive(Debug, Clone, Default, Serialize)]
pub struct StorageProfile {
    pub operations: BTreeMap<String, OperationProfile>,
}
pub(crate) type Sink = Arc<Mutex<Option<StorageProfile>>>;
pub(crate) struct ObservedTransaction<'a> {
    inner: Option<Transaction<'a>>,
    sink: Sink,
    enabled: bool,
    operation: &'static str,
    phase: &'static str,
    started: Option<Instant>,
    sample: OperationProfile,
    completed: bool,
}
impl<'a> ObservedTransaction<'a> {
    pub(crate) fn new(inner: Transaction<'a>, sink: Sink, operation: &'static str) -> Self {
        let enabled = sink.lock().is_ok_and(|s| s.is_some());
        Self {
            inner: Some(inner),
            sink,
            enabled,
            operation,
            phase: "authority",
            started: enabled.then(Instant::now),
            sample: Default::default(),
            completed: false,
        }
    }
    fn elapsed(&mut self) {
        if let Some(started) = self.started.replace(Instant::now()) {
            let p = self.sample.stages.entry(self.phase.into()).or_default();
            p.calls = p.calls.saturating_add(1);
            p.elapsed_us = p
                .elapsed_us
                .saturating_add(started.elapsed().as_micros() as u64);
        }
    }
    pub(crate) fn phase(&mut self, phase: &'static str) {
        if self.enabled {
            self.elapsed();
            self.phase = phase;
        }
    }
    pub(crate) fn clock(&self) -> Option<Instant> {
        self.enabled.then(Instant::now)
    }
    fn sql(&mut self, started: Option<Instant>) {
        if let Some(started) = started {
            let p = self.sample.stages.entry(self.phase.into()).or_default();
            p.sql_calls = p.sql_calls.saturating_add(1);
            p.sql_wait_us = p
                .sql_wait_us
                .saturating_add(started.elapsed().as_micros() as u64);
        }
    }
    pub(crate) fn verified(&mut self, started: Option<Instant>) {
        if let Some(started) = started {
            self.sample.artifact_verifications =
                self.sample.artifact_verifications.saturating_add(1);
            self.sample.artifact_verification_us = self
                .sample
                .artifact_verification_us
                .saturating_add(started.elapsed().as_micros() as u64);
        }
    }
    pub(crate) fn canonicalized(&mut self, started: Option<Instant>) {
        if let Some(started) = started {
            self.sample.artifact_canonicalizations =
                self.sample.artifact_canonicalizations.saturating_add(1);
            self.sample.artifact_canonicalization_us = self
                .sample
                .artifact_canonicalization_us
                .saturating_add(started.elapsed().as_micros() as u64);
        }
    }
    pub(crate) fn skipped(&mut self) {
        if self.enabled {
            self.sample.unchanged_artifacts_skipped =
                self.sample.unchanged_artifacts_skipped.saturating_add(1);
        }
    }
    pub(crate) fn query<T: ToStatement + ?Sized>(
        &mut self,
        q: &T,
        p: &[&(dyn ToSql + Sync)],
    ) -> Result<Vec<Row>, postgres::Error> {
        let t = self.clock();
        let r = self.inner.as_mut().unwrap().query(q, p);
        self.sql(t);
        r
    }
    pub(crate) fn query_one<T: ToStatement + ?Sized>(
        &mut self,
        q: &T,
        p: &[&(dyn ToSql + Sync)],
    ) -> Result<Row, postgres::Error> {
        let t = self.clock();
        let r = self.inner.as_mut().unwrap().query_one(q, p);
        self.sql(t);
        r
    }
    pub(crate) fn query_opt<T: ToStatement + ?Sized>(
        &mut self,
        q: &T,
        p: &[&(dyn ToSql + Sync)],
    ) -> Result<Option<Row>, postgres::Error> {
        let t = self.clock();
        let r = self.inner.as_mut().unwrap().query_opt(q, p);
        self.sql(t);
        r
    }
    pub(crate) fn execute<T: ToStatement + ?Sized>(
        &mut self,
        q: &T,
        p: &[&(dyn ToSql + Sync)],
    ) -> Result<u64, postgres::Error> {
        let t = self.clock();
        let r = self.inner.as_mut().unwrap().execute(q, p);
        self.sql(t);
        r
    }
    pub(crate) fn commit(mut self) -> Result<(), postgres::Error> {
        self.phase("commit");
        let t = self.clock();
        let r = self.inner.take().unwrap().commit();
        self.sql(t);
        self.completed = r.is_ok();
        r
    }
}
impl Drop for ObservedTransaction<'_> {
    fn drop(&mut self) {
        if !self.enabled {
            return;
        }
        self.elapsed();
        if let Ok(mut sink) = self.sink.lock()
            && let Some(profile) = sink.as_mut()
        {
            let p = profile.operations.entry(self.operation.into()).or_default();
            if self.completed {
                p.completed = p.completed.saturating_add(1)
            } else {
                p.aborted = p.aborted.saturating_add(1)
            }
            for (name, s) in &self.sample.stages {
                let target = p.stages.entry(name.clone()).or_default();
                target.calls = target.calls.saturating_add(s.calls);
                target.elapsed_us = target.elapsed_us.saturating_add(s.elapsed_us);
                target.sql_calls = target.sql_calls.saturating_add(s.sql_calls);
                target.sql_wait_us = target.sql_wait_us.saturating_add(s.sql_wait_us);
            }
            p.artifact_verifications = p
                .artifact_verifications
                .saturating_add(self.sample.artifact_verifications);
            p.artifact_verification_us = p
                .artifact_verification_us
                .saturating_add(self.sample.artifact_verification_us);
            p.artifact_canonicalizations = p
                .artifact_canonicalizations
                .saturating_add(self.sample.artifact_canonicalizations);
            p.artifact_canonicalization_us = p
                .artifact_canonicalization_us
                .saturating_add(self.sample.artifact_canonicalization_us);
            p.unchanged_artifacts_skipped = p
                .unchanged_artifacts_skipped
                .saturating_add(self.sample.unchanged_artifacts_skipped);
        }
    }
}
