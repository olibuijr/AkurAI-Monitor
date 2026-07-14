use axum::Json;
use axum::extract::{Path, Query};
use axum::http::StatusCode;
use rusqlite::types::{Value as SqlValue, ValueRef};
use rusqlite::{OptionalExtension, params_from_iter};
use serde::Deserialize;
use serde_json::{Map, Value, json};

use crate::{alert, db};

#[derive(Clone, Copy)]
struct Resource {
    name: &'static str,
    table: &'static str,
    fields: &'static [&'static str],
    required: &'static [&'static str],
    timestamped: bool,
    ordered_by_ts: bool,
}

const HOST_FIELDS: &[&str] = &[
    "name",
    "address",
    "kind",
    "enabled",
    "created_at",
    "updated_at",
];
const APPLICATION_FIELDS: &[&str] = &[
    "host_id",
    "name",
    "service_name",
    "health_url",
    "enabled",
    "created_at",
    "updated_at",
];
const METRIC_FIELDS: &[&str] = &["host_id", "application_id", "name", "value", "ts"];
const LOG_FIELDS: &[&str] = &["host_id", "application_id", "source", "line", "ts"];
const ALERT_FIELDS: &[&str] = &[
    "host_id",
    "application_id",
    "name",
    "metric_name",
    "operator",
    "threshold",
    "duration_secs",
    "enabled",
];
const RECIPIENT_FIELDS: &[&str] = &[
    "channel_id",
    "name",
    "target",
    "enabled",
    "created_at",
    "updated_at",
];
const CHANNEL_FIELDS: &[&str] = &[
    "name",
    "kind",
    "endpoint",
    "token_env",
    "enabled",
    "created_at",
    "updated_at",
];

fn resource(name: &str) -> Option<Resource> {
    Some(match name {
        "hosts" => Resource {
            name: "hosts",
            table: "hosts",
            fields: HOST_FIELDS,
            required: &["name", "address"],
            timestamped: true,
            ordered_by_ts: false,
        },
        "applications" => Resource {
            name: "applications",
            table: "applications",
            fields: APPLICATION_FIELDS,
            required: &["host_id", "name"],
            timestamped: true,
            ordered_by_ts: false,
        },
        "metrics" => Resource {
            name: "metrics",
            table: "metrics",
            fields: METRIC_FIELDS,
            required: &["name", "value"],
            timestamped: false,
            ordered_by_ts: true,
        },
        "logs" => Resource {
            name: "logs",
            table: "logs",
            fields: LOG_FIELDS,
            required: &["source", "line"],
            timestamped: false,
            ordered_by_ts: true,
        },
        "alerts" => Resource {
            name: "alerts",
            table: "alert_rules",
            fields: ALERT_FIELDS,
            required: &[
                "name",
                "metric_name",
                "operator",
                "threshold",
                "duration_secs",
            ],
            timestamped: false,
            ordered_by_ts: false,
        },
        "recipients" => Resource {
            name: "recipients",
            table: "recipients",
            fields: RECIPIENT_FIELDS,
            required: &["channel_id", "name", "target"],
            timestamped: true,
            ordered_by_ts: false,
        },
        "channels" => Resource {
            name: "channels",
            table: "channels",
            fields: CHANNEL_FIELDS,
            required: &["name", "kind", "endpoint"],
            timestamped: true,
            ordered_by_ts: false,
        },
        _ => return None,
    })
}

#[derive(Deserialize)]
pub struct ListQuery {
    limit: Option<usize>,
}

type ApiResponse = (StatusCode, Json<Value>);

pub async fn list(Path(name): Path<String>, Query(query): Query<ListQuery>) -> ApiResponse {
    let Some(resource) = resource(&name) else {
        return error(StatusCode::NOT_FOUND, "unknown resource");
    };
    let limit = query.limit.unwrap_or(200).clamp(1, 1000);
    let columns = select_columns(resource);
    let order = if resource.ordered_by_ts {
        "ts DESC"
    } else {
        "id DESC"
    };
    let sql = format!(
        "SELECT {columns} FROM {} ORDER BY {order} LIMIT ?1",
        resource.table
    );

    let result = db::with_db(|conn| {
        let mut statement = conn.prepare(&sql)?;
        let rows = statement.query_map([limit as i64], |row| row_json(row, resource))?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
    });

    match result {
        Ok(items) => (
            StatusCode::OK,
            Json(json!({"resource": resource.name, "count": items.len(), "items": items})),
        ),
        Err(_) => error(StatusCode::INTERNAL_SERVER_ERROR, "database query failed"),
    }
}

pub async fn get(Path((name, id)): Path<(String, i64)>) -> ApiResponse {
    let Some(resource) = resource(&name) else {
        return error(StatusCode::NOT_FOUND, "unknown resource");
    };
    let sql = format!(
        "SELECT {} FROM {} WHERE id = ?1",
        select_columns(resource),
        resource.table
    );
    let result = db::with_db(|conn| {
        conn.query_row(&sql, [id], |row| row_json(row, resource))
            .optional()
    });

    match result {
        Ok(Some(item)) => (
            StatusCode::OK,
            Json(json!({"resource": resource.name, "item": item})),
        ),
        Ok(None) => error(StatusCode::NOT_FOUND, "record not found"),
        Err(_) => error(StatusCode::INTERNAL_SERVER_ERROR, "database query failed"),
    }
}

pub async fn create(Path(name): Path<String>, Json(body): Json<Value>) -> ApiResponse {
    let Some(resource) = resource(&name) else {
        return error(StatusCode::NOT_FOUND, "unknown resource");
    };
    let Ok(mut fields) = object_body(body) else {
        return error(StatusCode::BAD_REQUEST, "request body must be an object");
    };
    apply_defaults(resource, &mut fields, true);
    if let Err(message) = validate(resource, &fields, true) {
        return error(StatusCode::BAD_REQUEST, message);
    }
    if resource.name == "channels"
        && let Err(message) = validate_channel_config(&fields, None)
    {
        return error(StatusCode::BAD_REQUEST, message);
    }

    let entries = ordered_entries(resource, &fields);
    let columns = entries
        .iter()
        .map(|(name, _)| *name)
        .collect::<Vec<_>>()
        .join(", ");
    let placeholders = (1..=entries.len())
        .map(|index| format!("?{index}"))
        .collect::<Vec<_>>()
        .join(", ");
    let values = match entries
        .iter()
        .map(|(_, value)| sql_value(value))
        .collect::<Result<Vec<_>, _>>()
    {
        Ok(values) => values,
        Err(message) => return error(StatusCode::BAD_REQUEST, message),
    };
    let sql = format!(
        "INSERT INTO {} ({columns}) VALUES ({placeholders})",
        resource.table
    );
    let result = db::with_db(|conn| {
        conn.execute(&sql, params_from_iter(values.iter()))?;
        Ok::<_, rusqlite::Error>(conn.last_insert_rowid())
    });

    match result {
        Ok(id) => get(Path((name, id))).await,
        Err(error_value) => database_error(error_value),
    }
}

pub async fn update(Path((name, id)): Path<(String, i64)>, Json(body): Json<Value>) -> ApiResponse {
    let Some(resource) = resource(&name) else {
        return error(StatusCode::NOT_FOUND, "unknown resource");
    };
    let Ok(mut fields) = object_body(body) else {
        return error(StatusCode::BAD_REQUEST, "request body must be an object");
    };
    if resource.name == "hosts"
        && fields
            .get("name")
            .is_some_and(|value| value.as_str() != Some("local"))
    {
        match is_local_host(id) {
            Ok(true) => return error(StatusCode::CONFLICT, "the local host cannot be renamed"),
            Ok(false) => {}
            Err(_) => return error(StatusCode::INTERNAL_SERVER_ERROR, "database query failed"),
        }
    }
    apply_defaults(resource, &mut fields, false);
    if fields.is_empty() {
        return error(StatusCode::BAD_REQUEST, "no fields to update");
    }
    if let Err(message) = validate(resource, &fields, false) {
        return error(StatusCode::BAD_REQUEST, message);
    }
    if resource.name == "channels"
        && let Err(message) = validate_channel_config(&fields, Some(id))
    {
        return error(StatusCode::BAD_REQUEST, message);
    }

    let entries = ordered_entries(resource, &fields);
    let assignments = entries
        .iter()
        .enumerate()
        .map(|(index, (name, _))| format!("{name} = ?{}", index + 1))
        .collect::<Vec<_>>()
        .join(", ");
    let mut values = match entries
        .iter()
        .map(|(_, value)| sql_value(value))
        .collect::<Result<Vec<_>, _>>()
    {
        Ok(values) => values,
        Err(message) => return error(StatusCode::BAD_REQUEST, message),
    };
    values.push(SqlValue::Integer(id));
    let sql = format!(
        "UPDATE {} SET {assignments} WHERE id = ?{}",
        resource.table,
        values.len()
    );
    let result = db::with_db(|conn| conn.execute(&sql, params_from_iter(values.iter())));

    match result {
        Ok(0) => error(StatusCode::NOT_FOUND, "record not found"),
        Ok(_) => get(Path((name, id))).await,
        Err(error_value) => database_error(error_value),
    }
}

pub async fn delete(Path((name, id)): Path<(String, i64)>) -> ApiResponse {
    let Some(resource) = resource(&name) else {
        return error(StatusCode::NOT_FOUND, "unknown resource");
    };
    if resource.name == "hosts" {
        match is_local_host(id) {
            Ok(true) => return error(StatusCode::CONFLICT, "the local host cannot be deleted"),
            Ok(false) => {}
            Err(_) => return error(StatusCode::INTERNAL_SERVER_ERROR, "database query failed"),
        }
    }
    let sql = format!("DELETE FROM {} WHERE id = ?1", resource.table);
    match db::with_db(|conn| conn.execute(&sql, [id])) {
        Ok(0) => error(StatusCode::NOT_FOUND, "record not found"),
        Ok(_) => (
            StatusCode::OK,
            Json(json!({"deleted": id, "resource": resource.name})),
        ),
        Err(error_value) => database_error(error_value),
    }
}

pub async fn test_channel(Path(id): Path<i64>) -> ApiResponse {
    match alert::test_channel(id).await {
        Ok(sent) => (
            StatusCode::OK,
            Json(json!({"channel_id": id, "sent": sent})),
        ),
        Err(message) => error(StatusCode::BAD_GATEWAY, &message),
    }
}

fn is_local_host(id: i64) -> rusqlite::Result<bool> {
    db::with_db(|conn| {
        conn.query_row("SELECT name FROM hosts WHERE id = ?1", [id], |row| {
            row.get::<_, String>(0)
        })
        .optional()
        .map(|name| name.as_deref() == Some("local"))
    })
}

fn select_columns(resource: Resource) -> String {
    std::iter::once("id")
        .chain(resource.fields.iter().copied())
        .collect::<Vec<_>>()
        .join(", ")
}

fn ordered_entries(resource: Resource, fields: &Map<String, Value>) -> Vec<(&'static str, &Value)> {
    resource
        .fields
        .iter()
        .filter_map(|field| fields.get(*field).map(|value| (*field, value)))
        .collect()
}

fn row_json(row: &rusqlite::Row<'_>, resource: Resource) -> rusqlite::Result<Value> {
    let mut object = Map::new();
    object.insert("id".to_string(), value_ref(row.get_ref(0)?));
    for (index, field) in resource.fields.iter().enumerate() {
        object.insert((*field).to_string(), value_ref(row.get_ref(index + 1)?));
    }
    Ok(Value::Object(object))
}

fn value_ref(value: ValueRef<'_>) -> Value {
    match value {
        ValueRef::Null => Value::Null,
        ValueRef::Integer(value) => Value::from(value),
        ValueRef::Real(value) => Value::from(value),
        ValueRef::Text(value) => Value::String(String::from_utf8_lossy(value).into_owned()),
        ValueRef::Blob(_) => Value::Null,
    }
}

fn sql_value(value: &Value) -> Result<SqlValue, &'static str> {
    match value {
        Value::Null => Ok(SqlValue::Null),
        Value::Bool(value) => Ok(SqlValue::Integer(i64::from(*value))),
        Value::Number(value) => value
            .as_i64()
            .map(SqlValue::Integer)
            .or_else(|| value.as_f64().map(SqlValue::Real))
            .ok_or("number is outside SQLite range"),
        Value::String(value) => Ok(SqlValue::Text(value.clone())),
        Value::Array(_) | Value::Object(_) => Err("field values must be scalar"),
    }
}

fn object_body(body: Value) -> Result<Map<String, Value>, ()> {
    body.as_object().cloned().ok_or(())
}

fn apply_defaults(resource: Resource, fields: &mut Map<String, Value>, create: bool) {
    let now = chrono::Utc::now().timestamp();
    if create {
        if resource.timestamped {
            fields.insert("created_at".to_string(), Value::from(now));
            fields.insert("updated_at".to_string(), Value::from(now));
        }
        if resource.ordered_by_ts {
            fields
                .entry("ts".to_string())
                .or_insert_with(|| Value::from(now));
        }
        if resource.fields.contains(&"enabled") {
            fields
                .entry("enabled".to_string())
                .or_insert(Value::from(true));
        }
        if resource.name == "hosts" {
            fields
                .entry("kind".to_string())
                .or_insert(Value::from("linux"));
        }
    } else if resource.timestamped {
        fields.insert("updated_at".to_string(), Value::from(now));
    }
}

fn validate(
    resource: Resource,
    fields: &Map<String, Value>,
    create: bool,
) -> Result<(), &'static str> {
    if let Some(field) = fields
        .keys()
        .find(|field| !resource.fields.contains(&field.as_str()))
    {
        let _ = field;
        return Err("request contains an unknown field");
    }
    if create
        && resource
            .required
            .iter()
            .any(|field| !fields.contains_key(*field) || fields[*field].is_null())
    {
        return Err("request is missing a required field");
    }
    if resource
        .required
        .iter()
        .any(|field| fields.get(*field).is_some_and(Value::is_null))
    {
        return Err("required fields cannot be null");
    }
    if fields
        .values()
        .any(|value| matches!(value, Value::Array(_) | Value::Object(_)))
    {
        return Err("field values must be scalar");
    }
    for (field, value) in fields {
        if value.is_null() {
            continue;
        }
        match field.as_str() {
            "enabled" => {
                let valid = value.is_boolean()
                    || value
                        .as_i64()
                        .is_some_and(|enabled| matches!(enabled, 0 | 1));
                if !valid {
                    return Err("enabled must be a boolean");
                }
            }
            "host_id" | "application_id" | "channel_id" | "duration_secs" => {
                if !value.as_i64().is_some_and(|number| number > 0) {
                    return Err("IDs and durations must be positive integers");
                }
            }
            "created_at" | "updated_at" | "ts" => {
                if value.as_i64().is_none() {
                    return Err("timestamps must be integers");
                }
            }
            "value" | "threshold" => {
                if !value.as_f64().is_some_and(f64::is_finite) {
                    return Err("metric values and thresholds must be finite numbers");
                }
            }
            _ if !value.is_string() => return Err("text fields must contain strings"),
            _ => {}
        }
    }
    if let Some(Value::String(operator)) = fields.get("operator")
        && !matches!(operator.as_str(), "gt" | "lt" | "eq")
    {
        return Err("operator must be gt, lt, or eq");
    }
    if let Some(Value::String(kind)) = fields.get("kind")
        && resource.name == "channels"
        && !matches!(kind.as_str(), "webhook" | "pi-bun")
    {
        return Err("channel kind must be webhook or pi-bun");
    }
    if let Some(Value::String(endpoint)) = fields.get("endpoint")
        && !endpoint.starts_with("http://")
        && !endpoint.starts_with("https://")
    {
        return Err("endpoint must be an HTTP or HTTPS URL");
    }
    Ok(())
}

fn validate_channel_config(
    fields: &Map<String, Value>,
    existing_id: Option<i64>,
) -> Result<(), &'static str> {
    let existing = existing_id
        .map(|id| {
            db::with_db(|conn| {
                conn.query_row(
                    "SELECT kind, endpoint, token_env FROM channels WHERE id = ?1",
                    [id],
                    |row| {
                        Ok((
                            row.get::<_, String>(0)?,
                            row.get::<_, String>(1)?,
                            row.get::<_, Option<String>>(2)?,
                        ))
                    },
                )
                .optional()
            })
        })
        .transpose()
        .map_err(|_| "failed to read channel")?
        .flatten();
    let kind = fields
        .get("kind")
        .and_then(Value::as_str)
        .or_else(|| existing.as_ref().map(|channel| channel.0.as_str()))
        .ok_or("channel kind is required")?;
    let endpoint = fields
        .get("endpoint")
        .and_then(Value::as_str)
        .or_else(|| existing.as_ref().map(|channel| channel.1.as_str()))
        .ok_or("channel endpoint is required")?;
    let token_env = match fields.get("token_env") {
        Some(Value::Null) => None,
        Some(Value::String(value)) => Some(value.as_str()),
        Some(_) => return Err("token_env must be a string or null"),
        None => existing.as_ref().and_then(|channel| channel.2.as_deref()),
    };
    if token_env.is_some_and(|name| !name.starts_with("MONITOR_CHANNEL_TOKEN_")) {
        return Err("channel tokens must use a MONITOR_CHANNEL_TOKEN_ environment variable");
    }

    let url = reqwest::Url::parse(endpoint).map_err(|_| "channel endpoint is not a valid URL")?;
    if !url.username().is_empty() || url.password().is_some() {
        return Err("channel endpoint cannot contain credentials");
    }
    match kind {
        "webhook" if url.scheme() != "https" => return Err("webhook endpoint must use HTTPS"),
        "pi-bun" => {
            if !matches!(url.scheme(), "http" | "https") || url.host_str() != Some("100.88.0.9") {
                return Err("pi-bun endpoint must use Titan's 100.88.0.9 mesh address");
            }
            if token_env.is_none() {
                return Err("pi-bun channel requires token_env");
            }
        }
        "webhook" => {}
        _ => return Err("channel kind must be webhook or pi-bun"),
    }
    Ok(())
}

fn database_error(error_value: rusqlite::Error) -> ApiResponse {
    match error_value {
        rusqlite::Error::SqliteFailure(sqlite_error, _)
            if sqlite_error.code == rusqlite::ErrorCode::ConstraintViolation =>
        {
            error(StatusCode::CONFLICT, "record conflicts with existing data")
        }
        _ => error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "database operation failed",
        ),
    }
}

fn error(status: StatusCode, message: &str) -> ApiResponse {
    (status, Json(json!({"error": message})))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validates_channel_transport_and_endpoint() {
        let channel = resource("channels").unwrap();
        let valid = json!({
            "name": "agents",
            "kind": "pi-bun",
            "endpoint": "http://100.88.0.9:4173",
            "token_env": "MONITOR_CHANNEL_TOKEN_PI_BUN",
        })
        .as_object()
        .unwrap()
        .clone();
        assert_eq!(validate(channel, &valid, true), Ok(()));
        assert_eq!(validate_channel_config(&valid, None), Ok(()));

        let mut invalid_kind = valid.clone();
        invalid_kind.insert("kind".to_string(), Value::from("shell"));
        assert_eq!(
            validate(channel, &invalid_kind, true),
            Err("channel kind must be webhook or pi-bun")
        );

        let mut wrong_type = valid.clone();
        wrong_type.insert("kind".to_string(), Value::from(true));
        assert_eq!(
            validate(channel, &wrong_type, true),
            Err("text fields must contain strings")
        );

        let mut wrong_pi_host = valid.clone();
        wrong_pi_host.insert("endpoint".to_string(), Value::from("http://127.0.0.1:4173"));
        assert_eq!(
            validate_channel_config(&wrong_pi_host, None),
            Err("pi-bun endpoint must use Titan's 100.88.0.9 mesh address")
        );

        let mut wrong_token_name = valid.clone();
        wrong_token_name.insert("token_env".to_string(), Value::from("WORKBENCH_TOKEN"));
        assert_eq!(
            validate_channel_config(&wrong_token_name, None),
            Err("channel tokens must use a MONITOR_CHANNEL_TOKEN_ environment variable")
        );

        let webhook = json!({
            "name": "webhook",
            "kind": "webhook",
            "endpoint": "http://127.0.0.1:18801/alert"
        })
        .as_object()
        .unwrap()
        .clone();
        assert_eq!(
            validate_channel_config(&webhook, None),
            Err("webhook endpoint must use HTTPS")
        );

        let mut invalid_endpoint = valid;
        invalid_endpoint.insert("endpoint".to_string(), Value::from("file:///tmp/socket"));
        assert_eq!(
            validate(channel, &invalid_endpoint, true),
            Err("endpoint must be an HTTP or HTTPS URL")
        );
    }

    #[test]
    fn validates_alert_operator_and_required_fields() {
        let alert = resource("alerts").unwrap();
        let mut fields = json!({
            "name": "High load",
            "metric_name": "load.5m",
            "operator": "gt",
            "threshold": 4,
            "duration_secs": 300
        })
        .as_object()
        .unwrap()
        .clone();
        assert_eq!(validate(alert, &fields, true), Ok(()));

        fields.insert("operator".to_string(), Value::from("contains"));
        assert_eq!(
            validate(alert, &fields, true),
            Err("operator must be gt, lt, or eq")
        );

        fields.insert("operator".to_string(), Value::from("gt"));
        fields.insert("duration_secs".to_string(), Value::from(0));
        assert_eq!(
            validate(alert, &fields, true),
            Err("IDs and durations must be positive integers")
        );
        fields.insert("duration_secs".to_string(), Value::from(300));
        fields.remove("metric_name");
        assert_eq!(
            validate(alert, &fields, true),
            Err("request is missing a required field")
        );
    }
}
