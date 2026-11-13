//! Internal work (spec 0012): the server side of `aq-internal-work`.
use crate::{AppState, authz::RequireOwner, error::AppError};
use aq_internal_work::{Error, http};
use axum::Router;
use sqlx::PgPool;
use uuid::Uuid;

impl AsRef<PgPool> for AppState {
    fn as_ref(&self) -> &PgPool {
        &self.store.pool
    }
}
impl From<RequireOwner> for Uuid {
    fn from(owner: RequireOwner) -> Uuid {
        owner.0.id
    }
}
impl From<Error> for AppError {
    fn from(e: Error) -> Self {
        match e {
            Error::Invalid(m) => Self::BadRequest(m),
            Error::Conflict(m) => Self::Conflict(m),
            Error::NotFound(m) => Self::NotFound(m),
            Error::Database(e) => crate::error::db(e),
            Error::Corrupt => Self::Internal,
        }
    }
}
pub(crate) fn admin_routes() -> Router<AppState> {
    http::routes::<AppState, RequireOwner, AppError>()
}
