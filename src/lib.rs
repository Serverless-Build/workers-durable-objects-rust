use futures_util::{pin_mut, StreamExt};
use serde::Deserialize;
use serde_json::{json, Value};
use worker::*;

const MARKER: &str = "SERVERLESS_BUILD_DURABLE_OBJECTS_RUST_V1";
const MIN: i64 = -1_000_000;
const MAX: i64 = 1_000_000;
const MAX_BODY_BYTES: usize = 1024;

#[derive(Deserialize)]
struct CountRow {
    value: i64,
}

fn reply(data: Value, status: u16) -> Result<Response> {
    let headers = Headers::new();
    headers.set("content-type", "application/json; charset=utf-8")?;
    headers.set("cache-control", "no-store")?;
    Ok(Response::from_json(&data)?
        .with_status(status)
        .with_headers(headers))
}

fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 40
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
}

#[event(fetch)]
async fn fetch(request: Request, env: Env, _ctx: Context) -> Result<Response> {
    let url = request.url()?;
    let path = url.path();
    if path == "/" || path == "/health" {
        if request.method() != Method::Get {
            return reply(json!({ "error": "Method not allowed" }), 405);
        }
        if path == "/health" {
            return reply(json!({ "ok": true, "marker": MARKER }), 200);
        }
        return reply(
            json!({
                "pattern": "SQLite-backed named counters", "runtime": "Rust", "marker": MARKER,
                "endpoints": ["GET /counter/{name}", "POST /counter/{name}/increment",
                    "POST /counter/{name}/decrement", "POST /counter/{name}/reset",
                    "POST /counter/{name}/set"],
                "limits": { "min": MIN, "max": MAX, "name": "1–40 ASCII letters, numbers, hyphens, or underscores" },
                "note": "Each name has independent, persistent storage. Demo names are public; use a unique name."
            }),
            200,
        );
    }

    let parts: Vec<&str> = path.split('/').collect();
    let (name, operation) = match parts.as_slice() {
        ["", "counter", name] => (*name, None),
        ["", "counter", name, operation]
            if ["increment", "decrement", "reset", "set"].contains(operation) =>
        {
            (*name, Some(*operation))
        }
        _ => return reply(json!({ "error": "Not found" }), 404),
    };
    if !valid_name(name) {
        return reply(
            json!({ "error": "Name must be 1–40 ASCII letters, numbers, hyphens, or underscores." }),
            400,
        );
    }
    if request.method()
        != if operation.is_some() {
            Method::Post
        } else {
            Method::Get
        }
    {
        return reply(json!({ "error": "Method not allowed" }), 405);
    }

    // The Rust SDK exposes a fetch handler on objects. Forward the original request
    // after validating the route; the object owns the synchronous SQL statements.
    env.durable_object("COUNTER")?
        .get_by_name(name)?
        .fetch_with_request(request)
        .await
}

#[durable_object]
pub struct Counter {
    state: State,
    initialized: bool,
}

impl DurableObject for Counter {
    fn new(state: State, _env: Env) -> Self {
        let sql = state.storage().sql();
        let initialized = sql
            .exec(
                "CREATE TABLE IF NOT EXISTS counter (id INTEGER PRIMARY KEY, value INTEGER NOT NULL)",
                None::<Vec<SqlStorageValue>>,
            )
            .and_then(|_| {
                sql.exec(
                    "INSERT OR IGNORE INTO counter (id, value) VALUES (1, 0)",
                    None::<Vec<SqlStorageValue>>,
                )
            })
            .is_ok();
        if !initialized {
            console_error!("Counter SQLite initialization failed");
        }
        Self { state, initialized }
    }

    async fn fetch(&self, mut request: Request) -> Result<Response> {
        if !self.initialized {
            return reply(json!({ "error": "Counter storage unavailable" }), 503);
        }
        let url = request.url()?;
        let parts: Vec<&str> = url.path().split('/').collect();
        let name = parts[2]; // The public Worker validated the path before forwarding it.
        let operation = parts.get(3).copied();
        let sql = self.state.storage().sql();

        let count = match operation {
            None => {
                sql.exec(
                    "SELECT value FROM counter WHERE id = 1",
                    None::<Vec<SqlStorageValue>>,
                )?
                .one::<CountRow>()?
                .value
            }
            Some("increment" | "decrement") => {
                let delta: i64 = if operation == Some("increment") {
                    1
                } else {
                    -1
                };
                let rows: Vec<CountRow> = sql
                    .exec(
                        "UPDATE counter SET value = value + ? WHERE id = 1 AND value + ? BETWEEN ? AND ? RETURNING value",
                        Some(vec![delta.into(), delta.into(), MIN.into(), MAX.into()]),
                    )?
                    .to_array()?;
                match rows.first() {
                    Some(row) => row.value,
                    None => {
                        return reply(
                            json!({ "error": format!("Counter must stay between {MIN} and {MAX}.") }),
                            409,
                        )
                    }
                }
            }
            Some("reset" | "set") => {
                let value = if operation == Some("reset") {
                    0
                } else {
                    match parse_set_value(&mut request).await? {
                        SetValue::Valid(value) => value,
                        SetValue::Invalid(status, message) => {
                            return reply(json!({ "error": message }), status)
                        }
                    }
                };
                sql.exec(
                    "UPDATE counter SET value = ? WHERE id = 1 RETURNING value",
                    Some(vec![value.into()]),
                )?
                .one::<CountRow>()?
                .value
            }
            _ => return reply(json!({ "error": "Not found" }), 404),
        };
        let mut result = json!({ "name": name, "count": count, "marker": MARKER });
        if let Some(operation) = operation {
            result["operation"] = json!(operation);
        }
        reply(result, 200)
    }
}

enum SetValue {
    Valid(i64),
    Invalid(u16, &'static str),
}

async fn parse_set_value(request: &mut Request) -> Result<SetValue> {
    let content_type = request.headers().get("content-type")?.unwrap_or_default();
    if !content_type
        .split(';')
        .next()
        .unwrap_or_default()
        .trim()
        .eq_ignore_ascii_case("application/json")
    {
        return Ok(SetValue::Invalid(
            400,
            "Send a JSON object with one integer value.",
        ));
    }
    if request
        .headers()
        .get("content-length")?
        .and_then(|length| length.parse::<usize>().ok())
        .is_some_and(|length| length > MAX_BODY_BYTES)
    {
        return Ok(SetValue::Invalid(413, "JSON body exceeds 1024 bytes."));
    }
    let mut bytes = Vec::new();
    let stream = request.stream()?;
    pin_mut!(stream);
    while let Some(chunk) = stream.next().await {
        let chunk = chunk?;
        if bytes.len() + chunk.len() > MAX_BODY_BYTES {
            return Ok(SetValue::Invalid(413, "JSON body exceeds 1024 bytes."));
        }
        bytes.extend(chunk);
    }
    let value = match serde_json::from_slice::<Value>(&bytes) {
        Ok(Value::Object(map)) if map.len() == 1 => map.get("value").and_then(Value::as_i64),
        _ => None,
    };
    Ok(match value.filter(|value| (MIN..=MAX).contains(value)) {
        Some(value) => SetValue::Valid(value),
        None => SetValue::Invalid(
            400,
            "Value must be an integer between -1000000 and 1000000.",
        ),
    })
}
