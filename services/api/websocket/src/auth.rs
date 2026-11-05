//! Verifies the tokens that identify a player on the user channel.
//!
//! Only a game server holds the secret, so a valid token is the game server saying who the player
//! is. Tokens are HS256 JWTs. `sub` is the player ID and `exp` is required.

use crate::protocol::valid_id;

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use ring::hmac;
use serde::Deserialize;

const MIN_SECRET_LEN: usize = 32;

#[derive(Deserialize)]
struct Header {
    alg: String,
}

#[derive(Deserialize)]
struct Claims {
    sub: String,
    exp: u64,
}

#[derive(Debug, PartialEq, Eq)]
pub struct Player {
    pub id: String,
    pub expires_at: u64,
}

#[derive(Clone)]
pub struct PlayerTokens {
    key: hmac::Key,
}

impl PlayerTokens {
    pub fn new(secret: &[u8]) -> Option<Self> {
        (secret.len() >= MIN_SECRET_LEN).then(|| Self {
            key: hmac::Key::new(hmac::HMAC_SHA256, secret),
        })
    }

    /// `None` covers every failure, so that clients can't tell which check failed.
    pub fn verify(&self, token: &str, now_secs: u64) -> Option<Player> {
        let mut parts = token.split('.');
        let (header, claims, signature) = (parts.next()?, parts.next()?, parts.next()?);
        if parts.next().is_some() {
            return None;
        }

        // The algorithm is fixed, so a token can't pick a weaker one such as `none`.
        let header_json = URL_SAFE_NO_PAD.decode(header).ok()?;
        let header: Header = serde_json::from_slice(&header_json).ok()?;
        if header.alg != "HS256" {
            return None;
        }

        let signature = URL_SAFE_NO_PAD.decode(signature).ok()?;
        let signed = &token[..token.rfind('.')?];
        hmac::verify(&self.key, signed.as_bytes(), &signature).ok()?;

        let claims: Claims = serde_json::from_slice(&URL_SAFE_NO_PAD.decode(claims).ok()?).ok()?;
        if claims.exp <= now_secs || !valid_id(&claims.sub) {
            return None;
        }

        Some(Player {
            id: claims.sub,
            expires_at: claims.exp,
        })
    }

    #[cfg(test)]
    pub fn sign(&self, header: &str, claims: &str) -> String {
        let signed = format!(
            "{}.{}",
            URL_SAFE_NO_PAD.encode(header),
            URL_SAFE_NO_PAD.encode(claims)
        );
        let signature = hmac::sign(&self.key, signed.as_bytes());
        format!("{}.{}", signed, URL_SAFE_NO_PAD.encode(signature.as_ref()))
    }
}
