//! Shared, owner-managed provider/class admission ceilings.
pub mod admission;
mod store;
use serde::{Deserialize, Serialize};
pub use store::{catalog, lock, operation, read, save, set_mode};

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("{0}")]
    Invalid(String),
    #[error("{0}")]
    Conflict(String),
    #[error("{0}")]
    NotFound(String),
    #[error("database")]
    Database(#[from] sqlx::Error),
    #[error("invalid stored swarm settings")]
    Internal,
}
pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    #[default]
    Normal,
    Reduced,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CellLimit {
    pub provider: String,
    pub intelligence_class: String,
    pub limit: u32,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Settings {
    pub revision: String,
    pub active_mode: Mode,
    pub normal: Option<Vec<CellLimit>>,
    pub reduced: Option<Vec<CellLimit>>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CatalogCell {
    pub provider: String,
    pub intelligence_class: String,
    pub display_name: String,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Catalog {
    pub revision: String,
    pub cells: Vec<CatalogCell>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SaveRequest {
    pub operation_id: uuid::Uuid,
    pub expected_revision: String,
    pub expected_catalog_revision: String,
    pub normal: Vec<CellLimit>,
    pub reduced: Option<Vec<CellLimit>>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModeRequest {
    pub operation_id: uuid::Uuid,
    pub expected_revision: String,
    pub active_mode: Mode,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Cell {
    #[serde(flatten)]
    pub catalog: CatalogCell,
    pub normal_limit: Option<u32>,
    pub reduced_limit: Option<u32>,
    pub effective_limit: Option<u32>,
    pub running: Option<u64>,
    pub waiting: Option<u64>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Snapshot {
    #[serde(flatten)]
    pub settings: Settings,
    pub catalog_revision: String,
    pub cells: Vec<Cell>,
    pub unclassified: Vec<Unclassified>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Unclassified {
    pub provider: String,
    pub running: u64,
    pub waiting: u64,
}
pub fn effective_limit(settings: &Settings, provider: &str, class: &str) -> Option<u32> {
    let matrix = match settings.active_mode {
        Mode::Normal => &settings.normal,
        Mode::Reduced => &settings.reduced,
    };
    cell_limit(matrix.as_deref(), provider, class)
}
fn cell_limit(matrix: Option<&[CellLimit]>, provider: &str, class: &str) -> Option<u32> {
    matrix?
        .iter()
        .find(|c| c.provider == provider && c.intelligence_class == class)
        .map(|c| c.limit)
}
fn invalid(message: &str) -> Error {
    Error::Invalid(message.into())
}
fn conflict(message: &str) -> Error {
    Error::Conflict(message.into())
}

fn validate(matrix: &[CellLimit], catalog: &Catalog) -> Result<()> {
    use std::collections::BTreeSet;
    let expected: BTreeSet<_> = catalog
        .cells
        .iter()
        .map(|c| (&c.provider, &c.intelligence_class))
        .collect();
    let actual: BTreeSet<_> = matrix
        .iter()
        .map(|c| (&c.provider, &c.intelligence_class))
        .collect();
    if actual.len() != matrix.len() || actual != expected || matrix.len() > 4096 {
        return Err(invalid(
            "Save one limit for every supported provider and intelligence class",
        ));
    }
    Ok(())
}
