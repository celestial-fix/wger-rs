use std::sync::Arc;

use sqlx::PgPool;

use crate::auth::JwtKeys;
use crate::config::Config;

#[derive(Clone)]
pub struct AppState {
    pub pool: PgPool,
    pub config: Config,
    pub keys: Arc<JwtKeys>,
}
