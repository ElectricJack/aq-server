//! AQ protection configuration. A caller supplies an authenticated TLS-configured
//! PostgreSQL client; no connection strings or key bytes enter errors/inspection.
use crate::Error;
use aq_playbooks_postgres::{PostgresStorage, ProtectionKeyring};
use nix::{libc, unistd::geteuid};
use serde::Deserialize;
use std::{
    collections::BTreeMap,
    fs::OpenOptions,
    io::Read,
    os::unix::{
        fs::PermissionsExt,
        fs::{MetadataExt, OpenOptionsExt},
    },
    path::Path,
    time::Duration,
};
use zeroize::{Zeroize, Zeroizing};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct KeyFile {
    active_key_id: String,
    keys: BTreeMap<String, String>,
}
impl Drop for KeyFile {
    fn drop(&mut self) {
        for value in self.keys.values_mut() {
            value.zeroize();
        }
    }
}
/// Reads an operator-owned regular private file through a no-follow fd. Directory
/// placement remains an operator responsibility; never use agent-writable roots.
pub fn read_keyring(path: &Path) -> Result<ProtectionKeyring, Error> {
    let bytes = key_file(path).ok_or(Error::Invalid)?;
    let config: KeyFile = serde_json::from_slice(&bytes).map_err(|_| Error::Invalid)?;
    config.decode().ok_or(Error::Invalid)
}

fn key_file(path: &Path) -> Option<Zeroizing<Vec<u8>>> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC)
        .open(path)
        .ok()?;
    let meta = file.metadata().ok()?;
    let owned = meta.is_file() && meta.uid() == geteuid().as_raw();
    let private = meta.permissions().mode() & 0o077 == 0 && meta.len() <= 4096;
    if !owned || !private {
        return None;
    }
    let mut bytes = Zeroizing::new(Vec::new());
    file.take(4097).read_to_end(&mut bytes).ok()?;
    if bytes.len() > 4096 {
        return None;
    }
    Some(bytes)
}

impl KeyFile {
    fn decode(mut self) -> Option<ProtectionKeyring> {
        let bounded =
            (1..=16).contains(&self.keys.len()) && self.keys.contains_key(&self.active_key_id);
        if !bounded || !self.keys.keys().all(|id| valid_id(id)) {
            return None;
        }
        let mut keys = BTreeMap::new();
        for (id, value) in &mut self.keys {
            let decoded = Zeroizing::new(hex::decode(&*value).ok()?);
            value.zeroize();
            keys.insert(id.clone(), decoded.as_slice().try_into().ok()?);
        }
        ProtectionKeyring::new(self.active_key_id.clone(), keys).ok()
    }
}

fn valid_id(id: &str) -> bool {
    (1..=128).contains(&id.len()) && !id.contains('\0')
}
/// Engine session deadlines, from `[playbooks.engine_timeouts]`: one engine
/// statement, one lock wait and the whole engine open, in milliseconds.
/// Defaults 30 s, 500 ms and 30 s.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Timeouts {
    pub statement_ms: u64,
    pub lock_ms: u64,
    /// How long one attempt to open the engine (its first load and owner
    /// probe) may take before it is abandoned and retried.
    pub open_ms: u64,
}
impl Default for Timeouts {
    fn default() -> Self {
        Self {
            statement_ms: 30_000,
            lock_ms: 500,
            open_ms: 30_000,
        }
    }
}
impl Timeouts {
    pub fn statement(&self) -> Duration {
        Duration::from_millis(self.statement_ms)
    }
    pub fn lock(&self) -> Duration {
        Duration::from_millis(self.lock_ms)
    }
    pub fn open(&self) -> Duration {
        Duration::from_millis(self.open_ms)
    }
    /// What [`storage`] accepts: positive, the lock wait at most the
    /// statement; and a positive open deadline. No upper bound.
    pub fn valid(&self) -> bool {
        self.statement_ms > 0 && (1..=self.statement_ms).contains(&self.lock_ms) && self.open_ms > 0
    }
    /// How long a caller waits for an owner call that makes `commits`
    /// commits: each may take the whole statement timeout, plus one request
    /// queued ahead of it and one lock wait. Never below the statement
    /// timeout, so a slow commit fails in PostgreSQL before the caller gives up.
    ///
    /// A HTTP route that makes one owner call can therefore wait this long on
    /// a stalled engine (60.5 s with the defaults), and the admission retry
    /// route makes two calls (lookup, then admit): up to twice that (121 s).
    /// A reverse proxy with a shorter idle timeout answers 504 first.
    pub fn call_deadline(&self, commits: u32) -> Duration {
        self.statement()
            .saturating_mul(commits.max(1).saturating_add(1))
            .saturating_add(self.lock())
    }
}
/// Call inside Owner::start's factory. Connection establishment must have its own
/// connect deadline and verified TLS policy. These bounded session deadlines are
/// applied before the embedded engine's schema migration/ownership work.
pub fn storage(
    mut client: postgres::Client,
    keyring: ProtectionKeyring,
    statement_timeout: Duration,
    lock_timeout: Duration,
) -> Result<PostgresStorage, Error> {
    let positive = statement_timeout.as_millis() > 0 && lock_timeout.as_millis() > 0;
    if !positive || lock_timeout > statement_timeout {
        return Err(Error::Invalid);
    }
    let mut configure = || -> Result<(), postgres::Error> {
        client.query_one(
            "SELECT set_config('statement_timeout',$1,false),set_config('lock_timeout',$2,false)",
            &[
                &format!("{}ms", statement_timeout.as_millis()),
                &format!("{}ms", lock_timeout.as_millis()),
            ],
        )?;
        Ok(())
    };
    configure().map_err(|_| Error::Engine("database session configuration failed".into()))?;
    PostgresStorage::from_client_protected(client, keyring)
        .map_err(|_| Error::Engine("protected namespace backend initialization failed".into()))
}
#[cfg(test)]
mod tests {
    use super::Timeouts;
    #[test]
    fn engine_timeouts_default_to_30_s_and_500_ms_and_are_bounded() {
        let parsed: Timeouts = serde_json::from_str("{}").unwrap();
        assert_eq!(parsed, Timeouts::default());
        assert_eq!(parsed.statement().as_millis(), 30_000);
        assert_eq!(parsed.lock().as_millis(), 500);
        assert!(parsed.valid());
        let set: Timeouts = serde_json::from_str(r#"{"statement_ms":2000,"lock_ms":100}"#).unwrap();
        assert!(set.valid() && set.statement().as_millis() == 2000);
        // No upper bound: a long legacy drain may need more than 30 s.
        let long = Timeouts {
            statement_ms: 120_000,
            lock_ms: 500,
            ..Timeouts::default()
        };
        assert!(long.valid());
        // A call waits for its own commits plus one queued request ahead.
        assert_eq!(long.call_deadline(1).as_millis(), 2 * 120_000 + 500);
        assert_eq!(
            Timeouts::default().call_deadline(8).as_millis(),
            9 * 30_000 + 500
        );
        for (statement, lock) in [(0, 0), (1000, 1001), (1000, 0)] {
            let t = Timeouts {
                statement_ms: statement,
                lock_ms: lock,
                ..Timeouts::default()
            };
            assert!(!t.valid(), "{statement} {lock}");
        }
    }
    #[test]
    fn the_engine_open_deadline_is_configuration_with_a_30_s_default() {
        assert_eq!(Timeouts::default().open().as_millis(), 30_000);
        let set: Timeouts = serde_json::from_str(r#"{"open_ms":300000}"#).unwrap();
        assert!(set.valid() && set.open().as_millis() == 300_000);
        let zero: Timeouts = serde_json::from_str(r#"{"open_ms":0}"#).unwrap();
        assert!(!zero.valid());
    }
}
