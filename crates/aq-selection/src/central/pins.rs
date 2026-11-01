use super::*;
use serde::Serialize;
use std::io::Write;
pub(super) fn bounded<T: Serialize>(v: &T, max: usize) -> Result<Vec<u8>> {
    struct Limited {
        bytes: Vec<u8>,
        max: usize,
    }
    impl Write for Limited {
        fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
            if self
                .bytes
                .len()
                .checked_add(b.len())
                .is_none_or(|n| n > self.max)
            {
                return Err(std::io::Error::other("central selection byte bound"));
            }
            self.bytes.extend_from_slice(b);
            Ok(b.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut w = Limited {
        bytes: Vec::new(),
        max,
    };
    serde_json::to_writer(&mut w, v)
        .map_err(|_| SelectionError::Invalid("central selection byte bound/encoding".into()))?;
    Ok(w.bytes)
}
pub(super) fn canonical<T: Serialize>(v: &T, max: usize) -> Result<Vec<u8>> {
    let bytes = bounded(v, max)?;
    let mut value: serde_json::Value = serde_json::from_slice(&bytes)
        .map_err(|_| SelectionError::Invalid("central selection encoding".into()))?;
    value.sort_all_objects();
    bounded(&value, max)
}
pub(super) fn reference<T: Serialize>(kind: &str, v: &T, max: usize) -> Result<ArtifactRef> {
    let bytes = canonical(v, max)?;
    let digest = Digest::of(&bytes);
    Ok(ArtifactRef {
        id: Text::new(format!("aq.central-selection.{kind}/{}", digest.as_str()))
            .map_err(|_| SelectionError::Invalid("central selection artifact id".into()))?,
        version: Text::new("1").unwrap(),
        digest,
    })
}
macro_rules! pinned {
    ($t:ty,$kind:literal,$max:expr) => {
        impl Pinned<$t> {
            /// Content-derived data pin only; the owner must authenticate the source.
            pub fn new(value: $t) -> Result<Self> {
                Ok(Self {
                    reference: reference($kind, &value, $max)?,
                    value,
                })
            }
            pub(super) fn verify(&self) -> Result<()> {
                ensure(
                    self.reference == reference($kind, &self.value, $max)?,
                    "central selection snapshot pin mismatch",
                )
            }
        }
    };
}
pinned!(ReservationPolicy, "reservation-policy", 16 * 1024);
pinned!(ReservationSnapshot, "reservation", 32 * 1024);
pinned!(CentralTurnPolicy, "policy", 64 * 1024);
pinned!(RegistrySnapshot, "registry", 256 * 1024);
pinned!(InstallationSnapshot, "installation", 256 * 1024);
