use axum::{
    extract::FromRequestParts,
    http::{HeaderMap, header::AUTHORIZATION, request::Parts},
};
use serde::Serialize;
use sha2::{Digest, Sha256};
use sqlx::Row;

use crate::{AppState, db::DbPool, error::ServerError};

#[derive(Clone, Debug)]
pub struct AuthenticatedUser {
    pub id: String,
    pub principal_id: String,
    pub email: String,
    pub name: Option<String>,
    pub is_server_admin: bool,
    pub orgs: Vec<OrgMembership>,
}

#[derive(Clone, Debug, Serialize)]
pub struct OrgMembership {
    pub slug: String,
    pub name: String,
    pub role: String,
}

impl FromRequestParts<AppState> for AuthenticatedUser {
    type Rejection = ServerError;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        let token = bearer_token(parts)?;
        let token_hash = hash_token(token);

        lookup_token_user(&state.db, &token_hash)
            .await
            .map_err(|err| {
                tracing::error!(error = %err, "token lookup failed");
                ServerError::internal_error()
            })?
            .ok_or_else(|| ServerError::unauthenticated("invalid bearer token"))
    }
}

pub fn hash_token(token: &str) -> String {
    hex_encode(&Sha256::digest(token.as_bytes()))
}

pub async fn authenticate_token(
    db: &DbPool,
    token: &str,
) -> Result<Option<AuthenticatedUser>, sqlx::Error> {
    let token_hash = hash_token(token);
    lookup_token_user(db, &token_hash).await
}

pub async fn authenticate_token_hash(
    db: &DbPool,
    token_hash: &str,
) -> Result<Option<AuthenticatedUser>, sqlx::Error> {
    lookup_token_user(db, token_hash).await
}

pub fn bearer_token_from_headers(headers: &HeaderMap) -> Result<Option<&str>, ServerError> {
    let Some(header) = headers.get(AUTHORIZATION) else {
        return Ok(None);
    };

    let Ok(value) = header.to_str() else {
        return Err(ServerError::unauthenticated("malformed bearer token"));
    };

    Ok(Some(parse_bearer_token(value)?))
}

async fn lookup_token_user(
    db: &DbPool,
    token_hash: &str,
) -> Result<Option<AuthenticatedUser>, sqlx::Error> {
    let rows = sqlx::query(
        "SELECT users.id, tokens.principal_id, users.email, users.name, users.is_server_admin,
                orgs.slug AS org_slug, orgs.name AS org_name, org_members.role AS org_role
         FROM tokens
         JOIN principals ON principals.id = tokens.principal_id
         JOIN human_profiles
           ON human_profiles.principal_id = tokens.principal_id
          AND human_profiles.user_id = tokens.user_id
         JOIN users ON users.id = tokens.user_id
         LEFT JOIN org_members ON org_members.user_id = users.id
         LEFT JOIN orgs ON orgs.id = org_members.org_id
         WHERE tokens.token_hash = $1
           AND tokens.token_kind = 'user'
           AND principals.principal_type = 'human'
           AND tokens.revoked_at IS NULL
           AND principals.disabled_at IS NULL
           AND (tokens.expires_at IS NULL
                OR tokens.expires_at > now())
         ORDER BY orgs.slug ASC",
    )
    .bind(token_hash)
    .fetch_all(db)
    .await?;

    if rows.is_empty() {
        return Ok(None);
    }

    if let Err(err) = sqlx::query(
        "UPDATE tokens
         SET last_used_at = now()
         WHERE token_hash = $1
           AND (last_used_at IS NULL OR last_used_at < now() - interval '1 minute')",
    )
    .bind(token_hash)
    .execute(db)
    .await
    {
        tracing::warn!(%err, "failed to update token last_used_at");
    }

    Ok(Some(user_from_rows(&rows)))
}

fn user_from_rows(rows: &[sqlx::postgres::PgRow]) -> AuthenticatedUser {
    let first = &rows[0];
    let orgs = rows
        .iter()
        .filter_map(|row| {
            let slug: Option<String> = row.get("org_slug");
            let name: Option<String> = row.get("org_name");
            let role: Option<String> = row.get("org_role");

            Some(OrgMembership {
                slug: slug?,
                name: name?,
                role: role?,
            })
        })
        .collect();

    AuthenticatedUser {
        id: first.get("id"),
        principal_id: first.get("principal_id"),
        email: first.get("email"),
        name: first.get("name"),
        is_server_admin: first.get("is_server_admin"),
        orgs,
    }
}

fn bearer_token(parts: &Parts) -> Result<&str, ServerError> {
    bearer_token_from_headers(&parts.headers)?
        .ok_or_else(|| ServerError::unauthenticated("missing bearer token"))
}

fn parse_bearer_token(value: &str) -> Result<&str, ServerError> {
    let Some((scheme, token)) = value.split_once(' ') else {
        return Err(ServerError::unauthenticated("malformed bearer token"));
    };
    if !scheme.eq_ignore_ascii_case("bearer")
        || token.is_empty()
        || token.chars().any(char::is_whitespace)
    {
        return Err(ServerError::unauthenticated("malformed bearer token"));
    }

    Ok(token)
}

fn hex_encode(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(HEX[(byte >> 4) as usize] as char);
        out.push(HEX[(byte & 0x0f) as usize] as char);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_bearer_token_accepts_case_insensitive_scheme() {
        assert_eq!(parse_bearer_token("Bearer t").unwrap(), "t");
        assert_eq!(parse_bearer_token("bearer t").unwrap(), "t");
        assert_eq!(parse_bearer_token("BEARER t").unwrap(), "t");
        assert!(parse_bearer_token("Foo t").is_err());
    }
}
