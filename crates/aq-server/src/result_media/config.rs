//! Explicit optional configuration. Absence mounts no media interface; upload
//! mutations remain disabled unless separately enabled.
use serde::Deserialize;
use std::{
    fmt,
    path::{Component, PathBuf},
};
use uuid::Uuid;

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MediaConfig {
    pub root: PathBuf,
    pub storage_id: Uuid,
    #[serde(default)]
    pub uploads_enabled: bool,
}
impl fmt::Debug for MediaConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ResultMediaConfig")
            .field("storage_id", &self.storage_id)
            .field("uploads_enabled", &self.uploads_enabled)
            .finish_non_exhaustive()
    }
}
impl MediaConfig {
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.storage_id.is_nil()
            || !self.root.is_absolute()
            || self
                .root
                .components()
                .any(|c| matches!(c, Component::ParentDir | Component::CurDir))
        {
            return Err("media root must be absolute and storage identity nonnil");
        }
        Ok(())
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn optin_is_closed_and_uploads_default_disabled() {
        let id = Uuid::new_v4();
        let config: MediaConfig =
            toml::from_str(&format!("root='/private/media'\nstorage_id='{id}'")).unwrap();
        config.validate().unwrap();
        assert!(!config.uploads_enabled);
        assert!(
            toml::from_str::<MediaConfig>(&format!(
                "root='/private/media'\nstorage_id='{id}'\nuploader_url='https://example.test'"
            ))
            .is_err()
        );
        let mut bad = config.clone();
        bad.root = "relative".into();
        assert!(bad.validate().is_err());
        bad = config;
        bad.storage_id = Uuid::nil();
        assert!(bad.validate().is_err());
    }
}
