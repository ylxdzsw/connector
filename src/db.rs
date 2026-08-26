use std::{path::Path, sync::Arc};

use anyhow::{Context, Result};
use chrono::Utc;
use rusqlite::{Connection, OptionalExtension, params};
use serde::Serialize;
use subtle::ConstantTimeEq;
use tokio::sync::Mutex;

use crate::crypto::{hash, pkce_s256, random_token};

const ACCESS_TTL: i64 = 60 * 60;
const REFRESH_TTL: i64 = 30 * 24 * 60 * 60;
const CODE_TTL: i64 = 5 * 60;

type RefreshRow = (i64, String, String, Option<i64>, Option<i64>);

#[derive(Clone)]
pub struct Database(Arc<Mutex<Connection>>);

#[derive(Debug, Clone, Serialize)]
pub struct ClientRecord {
    pub name: String,
    #[serde(skip_serializing)]
    pub code: String,
    pub expires_at: i64,
    pub revoked_at: Option<i64>,
}

#[derive(Debug, Clone, Serialize)]
pub struct GrantRecord {
    pub id: i64,
    pub subject: String,
    pub client_id: String,
    pub scopes: String,
    pub created_at: i64,
    pub revoked_at: Option<i64>,
}

#[derive(Debug, Clone)]
pub struct OAuthClient {
    pub client_id: String,
    pub redirect_uri: String,
}

#[derive(Debug, Clone)]
pub struct AuthCodeBinding {
    pub subject: String,
    pub client_id: String,
    pub redirect_uri: String,
    pub resource: String,
    pub scopes: String,
    pub code_challenge: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct TokenIssue {
    pub access_token: String,
    pub token_type: &'static str,
    pub expires_in: i64,
    pub scope: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub refresh_token: Option<String>,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum OAuthError {
    #[error("invalid client")]
    InvalidClient,
    #[error("invalid grant")]
    InvalidGrant,
    #[error("invalid request")]
    InvalidRequest,
    #[error("temporarily unavailable")]
    Storage,
}

impl Database {
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("create {}", parent.display()))?;
        }
        let connection =
            Connection::open(path).with_context(|| format!("open {}", path.display()))?;
        connection.pragma_update(None, "journal_mode", "WAL")?;
        connection.pragma_update(None, "foreign_keys", "ON")?;
        connection.busy_timeout(std::time::Duration::from_secs(5))?;
        connection.execute_batch(SCHEMA)?;
        Ok(Self(Arc::new(Mutex::new(connection))))
    }

    pub async fn register_oauth_client(
        &self,
        client_id: &str,
        secret: &str,
        redirect_uri: &str,
    ) -> Result<()> {
        let connection = self.0.lock().await;
        connection.execute(
            "INSERT INTO oauth_clients(client_id, secret_hash, redirect_uri) VALUES (?1, ?2, ?3)
             ON CONFLICT(client_id) DO UPDATE SET secret_hash=excluded.secret_hash, redirect_uri=excluded.redirect_uri",
            params![client_id, hash(secret).as_slice(), redirect_uri],
        )?;
        Ok(())
    }

    pub async fn oauth_client(&self, client_id: &str) -> Result<Option<OAuthClient>> {
        let connection = self.0.lock().await;
        Ok(connection
            .query_row(
                "SELECT client_id, redirect_uri FROM oauth_clients WHERE client_id=?1",
                [client_id],
                |row| {
                    Ok(OAuthClient {
                        client_id: row.get(0)?,
                        redirect_uri: row.get(1)?,
                    })
                },
            )
            .optional()?)
    }

    pub async fn authenticate_oauth_client(
        &self,
        client_id: &str,
        secret: &str,
    ) -> Result<Option<OAuthClient>> {
        let connection = self.0.lock().await;
        let row: Option<(Vec<u8>, String)> = connection
            .query_row(
                "SELECT secret_hash, redirect_uri FROM oauth_clients WHERE client_id=?1",
                [client_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        Ok(row.and_then(|(expected, redirect_uri)| {
            let actual = hash(secret);
            bool::from(actual.as_slice().ct_eq(&expected)).then(|| OAuthClient {
                client_id: client_id.to_owned(),
                redirect_uri,
            })
        }))
    }

    pub async fn create_client(&self, name: &str, code: &str, expires_at: i64) -> Result<()> {
        let connection = self.0.lock().await;
        connection.execute(
            "INSERT INTO clients(name, code, expires_at, created_at) VALUES (?1, ?2, ?3, ?4)",
            params![name, code, expires_at, now()],
        )?;
        Ok(())
    }

    pub async fn list_clients(&self) -> Result<Vec<ClientRecord>> {
        let connection = self.0.lock().await;
        let mut statement = connection.prepare(
            "SELECT name, code, expires_at, revoked_at FROM clients
                 ORDER BY (revoked_at IS NOT NULL), name",
        )?;
        let records = statement
            .query_map([], |row| {
                Ok(ClientRecord {
                    name: row.get(0)?,
                    code: row.get(1)?,
                    expires_at: row.get(2)?,
                    revoked_at: row.get(3)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(records)
    }

    pub async fn client_for_code(&self, code: &str) -> Result<Option<ClientRecord>> {
        let connection = self.0.lock().await;
        Ok(connection
            .query_row(
                "SELECT name, code, expires_at, revoked_at FROM clients
             WHERE code=?1 AND revoked_at IS NULL AND expires_at>?2",
                params![code, now()],
                |row| {
                    Ok(ClientRecord {
                        name: row.get(0)?,
                        code: row.get(1)?,
                        expires_at: row.get(2)?,
                        revoked_at: row.get(3)?,
                    })
                },
            )
            .optional()?)
    }

    pub async fn revoke_client(&self, name: &str) -> Result<bool> {
        let connection = self.0.lock().await;
        Ok(connection.execute(
            "UPDATE clients SET revoked_at=?2 WHERE name=?1 AND revoked_at IS NULL",
            params![name, now()],
        )? == 1)
    }

    pub async fn rotate_client_code(&self, name: &str, code: &str) -> Result<bool> {
        let connection = self.0.lock().await;
        Ok(connection.execute(
            "UPDATE clients SET code=?2, revoked_at=NULL WHERE name=?1",
            params![name, code],
        )? == 1)
    }

    pub async fn extend_client(&self, name: &str, days: u32) -> Result<bool> {
        let connection = self.0.lock().await;
        Ok(connection.execute(
            "UPDATE clients SET expires_at=MAX(expires_at, ?2) + ?3
             WHERE name=?1 AND revoked_at IS NULL",
            params![name, now(), i64::from(days) * 86_400],
        )? == 1)
    }

    pub async fn create_authorization_code(&self, binding: AuthCodeBinding) -> Result<String> {
        let raw_code = random_token();
        let connection = self.0.lock().await;
        let transaction = connection.unchecked_transaction()?;
        transaction.execute(
            "INSERT INTO grants(subject, client_id, scopes, created_at)
             VALUES (?1, ?2, ?3, ?4)",
            params![binding.subject, binding.client_id, binding.scopes, now()],
        )?;
        let grant_id = transaction.last_insert_rowid();
        transaction.execute(
            "INSERT INTO authorization_codes
             (code_hash, grant_id, subject, client_id, redirect_uri, resource, scopes, code_challenge, expires_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![hash(&raw_code).as_slice(), grant_id, binding.subject, binding.client_id,
                binding.redirect_uri, binding.resource, binding.scopes, binding.code_challenge, now() + CODE_TTL],
        )?;
        transaction.commit()?;
        Ok(raw_code)
    }

    pub async fn exchange_authorization_code(
        &self,
        code: &str,
        client_id: &str,
        redirect_uri: &str,
        resource: &str,
        verifier: &str,
    ) -> Result<TokenIssue, OAuthError> {
        let access = random_token();
        let refresh = random_token();
        let connection = self.0.lock().await;
        let tx = connection
            .unchecked_transaction()
            .map_err(|_| OAuthError::Storage)?;
        let binding: Option<(i64, String, String)> = tx
            .query_row(
                "SELECT grant_id, scopes, code_challenge FROM authorization_codes
             WHERE code_hash=?1 AND client_id=?2 AND redirect_uri=?3 AND resource=?4
               AND consumed_at IS NULL AND expires_at>?5",
                params![
                    hash(code).as_slice(),
                    client_id,
                    redirect_uri,
                    resource,
                    now()
                ],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .optional()
            .map_err(|_| OAuthError::Storage)?;
        let (grant_id, scopes, challenge) = binding.ok_or(OAuthError::InvalidGrant)?;
        if verifier.len() < 43
            || verifier.len() > 128
            || !bool::from(pkce_s256(verifier).as_bytes().ct_eq(challenge.as_bytes()))
        {
            return Err(OAuthError::InvalidGrant);
        }
        if tx.execute(
            "UPDATE authorization_codes SET consumed_at=?2 WHERE code_hash=?1 AND consumed_at IS NULL",
            params![hash(code).as_slice(), now()],
        ).map_err(|_| OAuthError::Storage)? != 1 {
            return Err(OAuthError::InvalidGrant);
        }
        insert_access(&tx, &access, grant_id, client_id, &scopes, resource)
            .map_err(|_| OAuthError::Storage)?;
        let refresh_token = if scope_has(&scopes, "offline_access") {
            insert_refresh(&tx, &refresh, grant_id, client_id, &scopes, resource)
                .map_err(|_| OAuthError::Storage)?;
            Some(refresh)
        } else {
            None
        };
        tx.commit().map_err(|_| OAuthError::Storage)?;
        Ok(TokenIssue {
            access_token: access,
            token_type: "Bearer",
            expires_in: ACCESS_TTL,
            scope: scopes,
            refresh_token,
        })
    }

    pub async fn rotate_refresh_token(
        &self,
        refresh_token: &str,
        client_id: &str,
        resource: &str,
    ) -> Result<TokenIssue, OAuthError> {
        let next_access = random_token();
        let next_refresh = random_token();
        let connection = self.0.lock().await;
        let tx = connection
            .unchecked_transaction()
            .map_err(|_| OAuthError::Storage)?;
        let row: Option<RefreshRow> = tx
            .query_row(
                "SELECT r.grant_id, r.scopes, r.resource, r.consumed_at, g.revoked_at
             FROM refresh_tokens r JOIN grants g ON g.id=r.grant_id
             WHERE r.token_hash=?1 AND r.client_id=?2 AND r.expires_at>?3",
                params![hash(refresh_token).as_slice(), client_id, now()],
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                    ))
                },
            )
            .optional()
            .map_err(|_| OAuthError::Storage)?;
        let (grant_id, scopes, bound_resource, consumed, revoked) =
            row.ok_or(OAuthError::InvalidGrant)?;
        if consumed.is_some() {
            tx.execute(
                "UPDATE grants SET revoked_at=?2 WHERE id=?1 AND revoked_at IS NULL",
                params![grant_id, now()],
            )
            .map_err(|_| OAuthError::Storage)?;
            tx.commit().map_err(|_| OAuthError::Storage)?;
            return Err(OAuthError::InvalidGrant);
        }
        if revoked.is_some() || bound_resource != resource {
            return Err(OAuthError::InvalidGrant);
        }
        if tx.execute(
            "UPDATE refresh_tokens SET consumed_at=?2 WHERE token_hash=?1 AND consumed_at IS NULL",
            params![hash(refresh_token).as_slice(), now()],
        ).map_err(|_| OAuthError::Storage)? != 1 {
            return Err(OAuthError::InvalidGrant);
        }
        insert_access(&tx, &next_access, grant_id, client_id, &scopes, resource)
            .map_err(|_| OAuthError::Storage)?;
        insert_refresh(&tx, &next_refresh, grant_id, client_id, &scopes, resource)
            .map_err(|_| OAuthError::Storage)?;
        tx.commit().map_err(|_| OAuthError::Storage)?;
        Ok(TokenIssue {
            access_token: next_access,
            token_type: "Bearer",
            expires_in: ACCESS_TTL,
            scope: scopes,
            refresh_token: Some(next_refresh),
        })
    }

    pub async fn validate_access_token(
        &self,
        token: &str,
        resource: &str,
        required_scope: &str,
    ) -> Result<bool> {
        let connection = self.0.lock().await;
        let scopes: Option<String> = connection
            .query_row(
                "SELECT a.scopes FROM access_tokens a JOIN grants g ON g.id=a.grant_id
             WHERE a.token_hash=?1 AND a.resource=?2 AND a.expires_at>?3
               AND a.revoked_at IS NULL AND g.revoked_at IS NULL",
                params![hash(token).as_slice(), resource, now()],
                |row| row.get(0),
            )
            .optional()?;
        Ok(scopes.is_some_and(|scopes| scope_has(&scopes, required_scope)))
    }

    pub async fn revoke_token(&self, token: &str, client_id: &str) -> Result<()> {
        let connection = self.0.lock().await;
        let tx = connection.unchecked_transaction()?;
        tx.execute(
            "UPDATE access_tokens SET revoked_at=?3 WHERE token_hash=?1 AND client_id=?2",
            params![hash(token).as_slice(), client_id, now()],
        )?;
        let grant: Option<i64> = tx
            .query_row(
                "SELECT grant_id FROM refresh_tokens WHERE token_hash=?1 AND client_id=?2",
                params![hash(token).as_slice(), client_id],
                |row| row.get(0),
            )
            .optional()?;
        if let Some(grant) = grant {
            tx.execute(
                "UPDATE grants SET revoked_at=?2 WHERE id=?1 AND revoked_at IS NULL",
                params![grant, now()],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    pub async fn list_grants(&self) -> Result<Vec<GrantRecord>> {
        let connection = self.0.lock().await;
        let mut statement = connection.prepare(
            "SELECT id, subject, client_id, scopes, created_at, revoked_at FROM grants ORDER BY id DESC"
        )?;
        Ok(statement
            .query_map([], |row| {
                Ok(GrantRecord {
                    id: row.get(0)?,
                    subject: row.get(1)?,
                    client_id: row.get(2)?,
                    scopes: row.get(3)?,
                    created_at: row.get(4)?,
                    revoked_at: row.get(5)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?)
    }

    pub async fn revoke_grant(&self, id: i64) -> Result<bool> {
        let connection = self.0.lock().await;
        Ok(connection.execute(
            "UPDATE grants SET revoked_at=?2 WHERE id=?1 AND revoked_at IS NULL",
            params![id, now()],
        )? == 1)
    }
}

fn insert_access(
    tx: &rusqlite::Transaction<'_>,
    token: &str,
    grant: i64,
    client: &str,
    scopes: &str,
    resource: &str,
) -> rusqlite::Result<()> {
    tx.execute(
        "INSERT INTO access_tokens(token_hash, grant_id, client_id, scopes, resource, expires_at, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        params![hash(token).as_slice(), grant, client, scopes, resource, now() + ACCESS_TTL, now()],
    )?;
    Ok(())
}

fn insert_refresh(
    tx: &rusqlite::Transaction<'_>,
    token: &str,
    grant: i64,
    client: &str,
    scopes: &str,
    resource: &str,
) -> rusqlite::Result<()> {
    tx.execute(
        "INSERT INTO refresh_tokens(token_hash, grant_id, client_id, scopes, resource, expires_at, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        params![hash(token).as_slice(), grant, client, scopes, resource, now() + REFRESH_TTL, now()],
    )?;
    Ok(())
}

pub fn scope_has(scopes: &str, required: &str) -> bool {
    scopes
        .split_ascii_whitespace()
        .any(|scope| scope == required)
}

fn now() -> i64 {
    Utc::now().timestamp()
}

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS clients (
    name TEXT PRIMARY KEY,
    code TEXT NOT NULL UNIQUE,
    expires_at INTEGER NOT NULL,
    created_at INTEGER NOT NULL,
    revoked_at INTEGER
);
CREATE TABLE IF NOT EXISTS oauth_clients (
    client_id TEXT PRIMARY KEY,
    secret_hash BLOB NOT NULL,
    redirect_uri TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS grants (
    id INTEGER PRIMARY KEY,
    subject TEXT NOT NULL,
    client_id TEXT NOT NULL REFERENCES oauth_clients(client_id),
    scopes TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    revoked_at INTEGER
);
CREATE TABLE IF NOT EXISTS authorization_codes (
    code_hash BLOB PRIMARY KEY,
    grant_id INTEGER NOT NULL REFERENCES grants(id),
    subject TEXT NOT NULL,
    client_id TEXT NOT NULL REFERENCES oauth_clients(client_id),
    redirect_uri TEXT NOT NULL,
    resource TEXT NOT NULL,
    scopes TEXT NOT NULL,
    code_challenge TEXT NOT NULL,
    expires_at INTEGER NOT NULL,
    consumed_at INTEGER
);
CREATE TABLE IF NOT EXISTS access_tokens (
    token_hash BLOB PRIMARY KEY,
    grant_id INTEGER NOT NULL REFERENCES grants(id),
    client_id TEXT NOT NULL REFERENCES oauth_clients(client_id),
    scopes TEXT NOT NULL,
    resource TEXT NOT NULL,
    expires_at INTEGER NOT NULL,
    created_at INTEGER NOT NULL,
    revoked_at INTEGER
);
CREATE TABLE IF NOT EXISTS refresh_tokens (
    token_hash BLOB PRIMARY KEY,
    grant_id INTEGER NOT NULL REFERENCES grants(id),
    client_id TEXT NOT NULL REFERENCES oauth_clients(client_id),
    scopes TEXT NOT NULL,
    resource TEXT NOT NULL,
    expires_at INTEGER NOT NULL,
    created_at INTEGER NOT NULL,
    consumed_at INTEGER
);
CREATE INDEX IF NOT EXISTS access_tokens_grant ON access_tokens(grant_id);
CREATE INDEX IF NOT EXISTS refresh_tokens_grant ON refresh_tokens(grant_id);
"#;

#[cfg(test)]
mod tests {
    use super::*;

    async fn setup() -> Database {
        let db = Database::open(":memory:").unwrap();
        db.register_oauth_client("chatgpt", "secret", "https://example.com/callback")
            .await
            .unwrap();
        db
    }

    #[tokio::test]
    async fn extends_active_and_expired_clients_but_not_revoked_clients() {
        let db = setup().await;
        let current = now();
        db.create_client("active", "AAAAAAAA", current + 100)
            .await
            .unwrap();
        db.create_client("expired", "BBBBBBBB", current - 100)
            .await
            .unwrap();
        db.create_client("revoked", "CCCCCCCC", current + 100)
            .await
            .unwrap();
        db.revoke_client("revoked").await.unwrap();

        assert!(db.extend_client("active", 2).await.unwrap());
        assert!(db.extend_client("expired", 2).await.unwrap());
        assert!(!db.extend_client("revoked", 2).await.unwrap());

        let clients = db.list_clients().await.unwrap();
        let active = clients
            .iter()
            .find(|client| client.name == "active")
            .unwrap();
        let expired = clients
            .iter()
            .find(|client| client.name == "expired")
            .unwrap();
        assert_eq!(active.expires_at, current + 100 + 2 * 86_400);
        assert!(expired.expires_at >= current + 2 * 86_400);
    }

    #[tokio::test]
    async fn rotates_client_code_reactivates_revoked_clients_and_sorts_them_last() {
        let db = setup().await;
        let current = now();
        db.create_client("z-active", "AAAAAAAA", current + 100)
            .await
            .unwrap();
        db.create_client("a-revoked", "BBBBBBBB", current + 100)
            .await
            .unwrap();
        db.revoke_client("a-revoked").await.unwrap();

        assert!(db.rotate_client_code("z-active", "CCCCCCCC").await.unwrap());
        assert!(db.client_for_code("AAAAAAAA").await.unwrap().is_none());
        assert_eq!(
            db.client_for_code("CCCCCCCC").await.unwrap().unwrap().name,
            "z-active"
        );

        let clients = db.list_clients().await.unwrap();
        assert_eq!(
            clients
                .iter()
                .map(|client| client.name.as_str())
                .collect::<Vec<_>>(),
            ["z-active", "a-revoked"]
        );

        assert!(
            db.rotate_client_code("a-revoked", "DDDDDDDD")
                .await
                .unwrap()
        );
        assert!(db.client_for_code("BBBBBBBB").await.unwrap().is_none());
        let restored = db.client_for_code("DDDDDDDD").await.unwrap().unwrap();
        assert_eq!(restored.name, "a-revoked");
        assert!(restored.revoked_at.is_none());
        assert_eq!(
            db.list_clients()
                .await
                .unwrap()
                .iter()
                .map(|client| client.name.as_str())
                .collect::<Vec<_>>(),
            ["a-revoked", "z-active"]
        );
    }

    #[tokio::test]
    async fn codes_are_single_use_and_pkce_bound() {
        let db = setup().await;
        let code = db
            .create_authorization_code(AuthCodeBinding {
                subject: "owner".into(),
                client_id: "chatgpt".into(),
                redirect_uri: "https://example.com/callback".into(),
                resource: "https://connector.test/mcp".into(),
                scopes: "control offline_access".into(),
                code_challenge: pkce_s256(&"x".repeat(43)),
            })
            .await
            .unwrap();
        assert_eq!(
            db.exchange_authorization_code(
                &code,
                "chatgpt",
                "https://example.com/callback",
                "https://connector.test/mcp",
                &"y".repeat(43)
            )
            .await
            .unwrap_err(),
            OAuthError::InvalidGrant
        );
        let issue = db
            .exchange_authorization_code(
                &code,
                "chatgpt",
                "https://example.com/callback",
                "https://connector.test/mcp",
                &"x".repeat(43),
            )
            .await
            .unwrap();
        assert!(issue.refresh_token.is_some());
        assert_eq!(
            db.exchange_authorization_code(
                &code,
                "chatgpt",
                "https://example.com/callback",
                "https://connector.test/mcp",
                &"x".repeat(43)
            )
            .await
            .unwrap_err(),
            OAuthError::InvalidGrant
        );
        assert!(
            db.validate_access_token(&issue.access_token, "https://connector.test/mcp", "control")
                .await
                .unwrap()
        );
    }

    #[tokio::test]
    async fn refresh_reuse_revokes_grant() {
        let db = setup().await;
        let verifier = "z".repeat(43);
        let code = db
            .create_authorization_code(AuthCodeBinding {
                subject: "owner".into(),
                client_id: "chatgpt".into(),
                redirect_uri: "https://example.com/callback".into(),
                resource: "https://connector.test/mcp".into(),
                scopes: "control offline_access".into(),
                code_challenge: pkce_s256(&verifier),
            })
            .await
            .unwrap();
        let first = db
            .exchange_authorization_code(
                &code,
                "chatgpt",
                "https://example.com/callback",
                "https://connector.test/mcp",
                &verifier,
            )
            .await
            .unwrap();
        let old_refresh = first.refresh_token.unwrap();
        let second = db
            .rotate_refresh_token(&old_refresh, "chatgpt", "https://connector.test/mcp")
            .await
            .unwrap();
        assert_eq!(
            db.rotate_refresh_token(&old_refresh, "chatgpt", "https://connector.test/mcp")
                .await
                .unwrap_err(),
            OAuthError::InvalidGrant
        );
        assert!(
            !db.validate_access_token(
                &second.access_token,
                "https://connector.test/mcp",
                "control"
            )
            .await
            .unwrap()
        );
    }
}
