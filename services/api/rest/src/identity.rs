//! Who a request acts for. Every route takes a `Player`, so it doesn't need to know how the player
//! was established.
//!
//! Today game servers are trusted to name the player, and the firewall is what makes that safe.
//! OIDC is another `Authenticator`: validate the bearer token, map the user to an account, and
//! hand it to `AppState` in `main.rs`. No route needs to change.

use super::AppState;
use super::errors::AppError;
use super::order::{UuidType, uuid_to_u64s};

use std::sync::Arc;

use async_trait::async_trait;

use axum::extract::FromRequestParts;
use axum::http::request::Parts;

use uuid::Uuid;

pub const PLAYER_HEADER: &str = "x-player-id";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Player(pub Uuid);

impl Player {
    pub fn account(self) -> UuidType {
        uuid_to_u64s(self.0)
    }
}

#[async_trait]
pub trait Authenticator: Send + Sync {
    async fn authenticate(&self, request: &Parts) -> Result<Player, AppError>;
}

/// Takes the player from a header set by a game server.
pub struct TrustedGameServer;

#[async_trait]
impl Authenticator for TrustedGameServer {
    async fn authenticate(&self, request: &Parts) -> Result<Player, AppError> {
        let header = request
            .headers
            .get(PLAYER_HEADER)
            .ok_or_else(|| AppError::Unauthenticated(format!("{PLAYER_HEADER} is required.")))?;
        header
            .to_str()
            .ok()
            .and_then(|value| Uuid::parse_str(value).ok())
            .map(Player)
            .ok_or_else(|| AppError::Unauthenticated(format!("{PLAYER_HEADER} must be a UUID.")))
    }
}

impl FromRequestParts<Arc<AppState>> for Player {
    type Rejection = AppError;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &Arc<AppState>,
    ) -> Result<Self, Self::Rejection> {
        state.auth.authenticate(parts).await
    }
}
