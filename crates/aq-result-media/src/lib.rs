//! Pure supplemental-media validation and deterministic PNG rendering.
//! No SQL, filesystem, HTTP, authentication, execution or result-owner grants.
//! A well-formed report binding is data; only the daemon can resolve its owner.
#![forbid(unsafe_code)]
mod codec;
mod domain;
mod limits;

pub use codec::{Header, PreparedImage, PreparedVariant, preflight_png, prepare_png};
pub use domain::*;
pub use limits::*;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Invalid {
    Field(&'static str),
    Limit(&'static str),
    UnsupportedPng,
    MalformedPng,
    ProcessingFailure,
}
impl std::fmt::Display for Invalid {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Field(name) => write!(f, "invalid media field: {name}"),
            Self::Limit(name) => write!(f, "media limit exceeded: {name}"),
            Self::UnsupportedPng => {
                f.write_str("PNG is outside the supported static RGB/RGBA8 subset")
            }
            Self::MalformedPng => f.write_str("PNG validation failed"),
            Self::ProcessingFailure => f.write_str("PNG processing failed"),
        }
    }
}
impl std::error::Error for Invalid {}
pub type Result<T> = std::result::Result<T, Invalid>;
pub trait Validate {
    fn validate(&self) -> Result<()>;
}
pub(crate) fn require(value: bool, name: &'static str) -> Result<()> {
    if value {
        Ok(())
    } else {
        Err(Invalid::Field(name))
    }
}
