use serde::Serialize;
use sqlx::Row;

use crate::db::DbPool;

#[derive(Debug, Serialize)]
pub struct AuditEvent {
    pub id: String,
    pub org: String,
    pub action: String,
    pub resource_type: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resource_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resource: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub actor_email: Option<String>,
    pub metadata: serde_json::Value,
    pub created_at: String,
}

pub async fn list_events(
    db: &DbPool,
    org: &str,
    resource_filter: Option<(&str, &str)>,
    event_id: Option<&str>,
    limit: Option<i64>,
) -> Result<Vec<AuditEvent>, sqlx::Error> {
    let rows = sqlx::query(
        "SELECT audit_log.id, orgs.slug AS org_slug, users.email AS actor_email,
                audit_log.action, audit_log.resource_type, audit_log.resource_id,
                audit_log.metadata::text AS metadata_json, audit_log.created_at::text AS created_at,
                CASE
                    WHEN audit_log.resource_type = 'skill' THEN skills.name
                    WHEN audit_log.resource_type = 'stack' THEN stacks.slug
                    WHEN audit_log.resource_type = 'team' THEN teams.slug
                    ELSE NULL
                END AS resource_slug
         FROM audit_log
         JOIN orgs ON orgs.id = audit_log.org_id
         LEFT JOIN users ON users.id = audit_log.actor_user_id
         LEFT JOIN skills ON skills.id = audit_log.resource_id
              AND audit_log.resource_type = 'skill'
         LEFT JOIN stacks ON stacks.id = audit_log.resource_id
              AND audit_log.resource_type = 'stack'
         LEFT JOIN teams ON teams.id = audit_log.resource_id
              AND audit_log.resource_type = 'team'
         WHERE orgs.slug = $1
           AND ($2 IS NULL OR audit_log.resource_type = $3)
           AND ($4 IS NULL OR audit_log.resource_id = $5)
           AND ($6 IS NULL OR audit_log.id = $7)
         ORDER BY audit_log.created_at DESC, audit_log.id DESC
         LIMIT $8",
    )
    .bind(org)
    .bind(resource_filter.map(|filter| filter.0))
    .bind(resource_filter.map(|filter| filter.0))
    .bind(resource_filter.map(|filter| filter.1))
    .bind(resource_filter.map(|filter| filter.1))
    .bind(event_id)
    .bind(event_id)
    .bind(limit)
    .fetch_all(db)
    .await?;

    Ok(rows
        .into_iter()
        .map(|row| {
            let metadata_json: Option<String> = row.get("metadata_json");
            let metadata = metadata_json
                .as_deref()
                .and_then(|value| serde_json::from_str(value).ok())
                .map(redact_metadata)
                .unwrap_or_else(|| serde_json::json!({}));
            AuditEvent {
                id: row.get("id"),
                org: row.get("org_slug"),
                action: row.get("action"),
                resource_type: row.get("resource_type"),
                resource_id: row.get("resource_id"),
                resource: row.get("resource_slug"),
                actor_email: row.get("actor_email"),
                metadata,
                created_at: row.get("created_at"),
            }
        })
        .collect())
}

pub async fn show_event(
    db: &DbPool,
    org: &str,
    event_id: &str,
) -> Result<Option<AuditEvent>, sqlx::Error> {
    let mut events = list_events(db, org, None, Some(event_id), Some(1)).await?;
    Ok(events.pop())
}

pub fn redact_metadata(value: serde_json::Value) -> serde_json::Value {
    match value {
        serde_json::Value::Object(map) => serde_json::Value::Object(
            map.into_iter()
                .map(|(key, value)| {
                    let value = if is_sensitive_key(&key) {
                        serde_json::Value::String("[REDACTED]".to_string())
                    } else {
                        redact_metadata(value)
                    };
                    (key, value)
                })
                .collect(),
        ),
        serde_json::Value::Array(values) => {
            serde_json::Value::Array(values.into_iter().map(redact_metadata).collect())
        }
        other => other,
    }
}

fn is_sensitive_key(key: &str) -> bool {
    let compact: String = key
        .chars()
        .filter(|ch| *ch != '_' && *ch != '-' && !ch.is_whitespace())
        .flat_map(char::to_lowercase)
        .collect();
    [
        "token",
        "authorization",
        "bearer",
        "secret",
        "password",
        "credential",
        "apikey",
        "key",
        "session",
    ]
    .iter()
    .any(|needle| compact.contains(needle))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn redacts_likely_secret_keys_recursively() {
        let redacted = redact_metadata(json!({
            "token": "raw",
            "nested": {
                "api_key": "key",
                "private_key": "private",
                "session_id": "session",
                "safe": "value",
                "items": [
                    { "Authorization": "Bearer raw" },
                    { "count": 1 }
                ]
            }
        }));

        assert_eq!(redacted["token"], "[REDACTED]");
        assert_eq!(redacted["nested"]["api_key"], "[REDACTED]");
        assert_eq!(redacted["nested"]["private_key"], "[REDACTED]");
        assert_eq!(redacted["nested"]["session_id"], "[REDACTED]");
        assert_eq!(
            redacted["nested"]["items"][0]["Authorization"],
            "[REDACTED]"
        );
        assert_eq!(redacted["nested"]["safe"], "value");
        assert_eq!(redacted["nested"]["items"][1]["count"], 1);
    }
}
