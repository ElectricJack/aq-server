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
    let load = || -> Option<ProtectionKeyring> {
        let file = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC)
            .open(path)
            .ok()?;
        let meta = file.metadata().ok()?;
        if !meta.is_file()
            || meta.uid() != geteuid().as_raw()
            || meta.permissions().mode() & 0o077 != 0
            || meta.len() > 4096
        {
            return None;
        }
        let mut bytes = Zeroizing::new(Vec::new());
        file.take(4097).read_to_end(&mut bytes).ok()?;
        if bytes.len() > 4096 {
            return None;
        }
        let mut config: KeyFile = serde_json::from_slice(&bytes).ok()?;
        if config.keys.is_empty()
            || config.keys.len() > 16
            || !config.keys.contains_key(&config.active_key_id)
            || config
                .keys
                .keys()
                .any(|id| id.is_empty() || id.len() > 128 || id.contains('\0'))
        {
            return None;
        }
        let mut keys = BTreeMap::new();
        for (id, value) in &mut config.keys {
            let decoded = Zeroizing::new(hex::decode(&*value).ok()?);
            value.zeroize();
            keys.insert(id.clone(), decoded.as_slice().try_into().ok()?);
        }
        ProtectionKeyring::new(config.active_key_id.clone(), keys).ok()
    };
    load().ok_or(Error::Invalid)
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
    if statement_timeout.as_millis() == 0
        || statement_timeout > Duration::from_secs(30)
        || lock_timeout.as_millis() == 0
        || lock_timeout > statement_timeout
    {
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
