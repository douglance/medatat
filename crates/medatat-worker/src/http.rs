//! Response construction and request-line parsing.
//!
//! Every status code and every envelope in `docs/03-API.md` is produced here and nowhere
//! else, so the contract is one file to read and one file to test. Nothing in this module
//! touches workerd: [`Rendered`] is a status plus an optional JSON body, and `routes/`
//! turns it into a `worker::Response` in a single place.

use crate::error::LogicError;
use medatat_core::ids::CaseRev;
use medatat_core::wire::{ApiError, Envelope, ErrorCode, PutValuesResp};
use serde::Serialize;

/// A fully decided response. `body: None` means no body at all — 204 and 304 carry none.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rendered {
    pub status: u16,
    pub body: Option<String>,
}

impl Rendered {
    pub fn json(status: u16, body: String) -> Self {
        Rendered {
            status,
            body: Some(body),
        }
    }

    pub fn empty(status: u16) -> Self {
        Rendered { status, body: None }
    }

    /// The parsed body, for tests. Panics only in test code.
    #[cfg(test)]
    fn value(&self) -> serde_json::Value {
        serde_json::from_str(self.body.as_deref().expect("a body")).expect("valid json")
    }
}

fn envelope<T: Serialize>(status: u16, env: &Envelope<T>) -> Rendered {
    match serde_json::to_string(env) {
        Ok(body) => Rendered::json(status, body),
        // Encoding our own response cannot fail on well-formed data; if it does, the client
        // still gets a shaped envelope rather than a truncated body.
        Err(e) => Rendered::json(
            500,
            format!(
                r#"{{"ok":false,"error":{{"code":"internal","message":"response encoding failed: {}"}}}}"#,
                e.to_string().replace('"', "'")
            ),
        ),
    }
}

/// `200` with `{ok: true, data}`.
pub fn ok<T: Serialize>(data: T) -> Rendered {
    envelope(200, &Envelope::ok(data))
}

/// `201` with `{ok: true, data}` — `POST /cases` only.
pub fn created<T: Serialize>(data: T) -> Rendered {
    envelope(201, &Envelope::ok(data))
}

/// `204`. `POST /auth/request` answers this for every email, known or not.
pub fn no_content() -> Rendered {
    Rendered::empty(204)
}

/// `304` — the client's `since_rev` is already current.
pub fn not_modified() -> Rendered {
    Rendered::empty(304)
}

/// The one place a [`LogicError`] becomes a status and a body.
pub fn error(err: LogicError) -> Rendered {
    let status = err.http_status();
    envelope(status, &Envelope::<()>::err(err.into_api_error()))
}

pub fn api_error(code: ErrorCode, message: impl Into<String>) -> Rendered {
    envelope(
        code.http_status(),
        &Envelope::<()>::err(ApiError::new(code, message)),
    )
}

/// `POST /cases/{id}/values`. Applied is a `200` success envelope; a conflict is a `409`
/// *error* envelope whose `detail` carries the server's rows, exactly as documented.
pub fn put_values(resp: PutValuesResp) -> Rendered {
    match resp {
        PutValuesResp::Applied { rev, applied } => ok(PutValuesResp::Applied { rev, applied }),
        PutValuesResp::Conflict {
            server_rev,
            conflicts,
        } => {
            let detail = serde_json::json!({
                "server_rev": server_rev,
                "conflicts": conflicts,
            });
            envelope(
                409,
                &Envelope::<()>::err(
                    ApiError::new(
                        ErrorCode::Conflict,
                        "one or more fields changed on the server since base_rev",
                    )
                    .with_detail(detail),
                ),
            )
        }
    }
}

// ------------------------------------------------------------------ request line

/// One query parameter, without pulling a URL parser into the hot path. Values are ids,
/// integers, and email-free words; `+` is decoded as a space and `%XX` as a byte.
pub fn query_value(query: &str, key: &str) -> Option<String> {
    query
        .trim_start_matches('?')
        .split('&')
        .filter(|p| !p.is_empty())
        .find_map(|pair| {
            let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
            (percent_decode(k) == key).then(|| percent_decode(v))
        })
}

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b'%' if i + 2 < bytes.len() => {
                let hex = |b: u8| (b as char).to_digit(16);
                match (hex(bytes[i + 1]), hex(bytes[i + 2])) {
                    (Some(h), Some(l)) => {
                        out.push((h * 16 + l) as u8);
                        i += 3;
                    }
                    _ => {
                        out.push(b'%');
                        i += 1;
                    }
                }
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// `?since_rev=<n>`. Absent reads the whole case; unparseable is a client error rather than
/// a silent full read, which would quietly ship every value in the case.
pub fn parse_since_rev(query: &str) -> Result<Option<CaseRev>, LogicError> {
    parse_i64(query, "since_rev").map(|o| o.map(CaseRev))
}

pub fn parse_i64(query: &str, key: &str) -> Result<Option<i64>, LogicError> {
    match query_value(query, key) {
        None => Ok(None),
        Some(raw) if raw.is_empty() => Ok(None),
        Some(raw) => raw
            .parse::<i64>()
            .map(Some)
            .map_err(|_| LogicError::Validation(format!("{key} must be an integer, got {raw:?}"))),
    }
}

pub fn parse_u32(query: &str, key: &str) -> Result<Option<u32>, LogicError> {
    match parse_i64(query, key)? {
        None => Ok(None),
        Some(n) if (0..=u32::MAX as i64).contains(&n) => Ok(Some(n as u32)),
        Some(n) => Err(LogicError::Validation(format!("{key} out of range: {n}"))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use medatat_core::ids::FieldId;
    use medatat_core::value::{Value, parse_time_24};
    use medatat_core::wire::{Health, ValueRow};

    #[test]
    fn success_envelope_matches_the_documented_shape() {
        let r = ok(Health {
            version: "0.1.0".into(),
            config_rev: medatat_core::ids::ConfigRev(41),
        });
        assert_eq!(r.status, 200);
        let v = r.value();
        assert_eq!(v["ok"], serde_json::json!(true));
        assert_eq!(v["data"]["config_rev"], serde_json::json!(41));
        assert!(v.get("error").is_none(), "no error key on success: {v}");
    }

    #[test]
    fn error_envelope_matches_the_documented_shape() {
        let r = error(LogicError::NotFound("case".into()));
        assert_eq!(r.status, 404);
        let v = r.value();
        assert_eq!(v["ok"], serde_json::json!(false));
        assert_eq!(v["error"]["code"], serde_json::json!("not_found"));
        assert!(v["error"]["message"].is_string());
        assert!(v.get("data").is_none(), "no data key on failure: {v}");
    }

    #[test]
    fn every_documented_error_code_keeps_its_status() {
        for (err, status, code) in [
            (LogicError::Unauthorized, 401, "unauthorized"),
            (LogicError::Forbidden("x".into()), 403, "forbidden"),
            (LogicError::NotFound("x".into()), 404, "not_found"),
            (LogicError::Validation("x".into()), 422, "validation"),
            (LogicError::RateLimited, 429, "rate_limited"),
            (LogicError::Storage("x".into()), 500, "internal"),
        ] {
            let r = error(err);
            assert_eq!(r.status, status);
            assert_eq!(r.value()["error"]["code"], serde_json::json!(code));
        }
    }

    #[test]
    fn r16_applied_is_a_200_success_envelope() {
        let id = FieldId::new();
        let r = put_values(PutValuesResp::Applied {
            rev: CaseRev(13),
            applied: vec![id],
        });
        assert_eq!(r.status, 200);
        let v = r.value();
        assert_eq!(v["ok"], serde_json::json!(true));
        assert_eq!(v["data"]["rev"], serde_json::json!(13));
        assert_eq!(v["data"]["applied"][0], serde_json::json!(id));
    }

    #[test]
    fn r16_conflict_is_a_409_carrying_the_server_rows() {
        let id = FieldId::new();
        let r = put_values(PutValuesResp::Conflict {
            server_rev: CaseRev(15),
            conflicts: vec![ValueRow {
                field_id: id,
                value: Value::Time(parse_time_24("09:30").unwrap()),
                rev: CaseRev(15),
                updated_by: Some(medatat_core::ids::ActorId::new("u-2")),
                updated_at: Some("2026-08-17T09:30:00Z".into()),
            }],
        });
        assert_eq!(r.status, 409);
        let v = r.value();
        assert_eq!(v["ok"], serde_json::json!(false));
        assert_eq!(v["error"]["code"], serde_json::json!("conflict"));
        assert_eq!(v["error"]["detail"]["server_rev"], serde_json::json!(15));
        let row = &v["error"]["detail"]["conflicts"][0];
        assert_eq!(row["field_id"], serde_json::json!(id));
        // `HH:MM`, exactly as `docs/03-API.md` prints it and exactly as R8 stores it.
        // `medatat-core` serialises `Value::Time` through `format_time_24`, so the wire
        // form, the storage form, and the documented form are one string.
        assert_eq!(row["value"], serde_json::json!({ "Time": "09:30" }));
        assert_eq!(row["updated_by"], serde_json::json!("u-2"));
    }

    #[test]
    fn a_malformed_numeric_parameter_is_an_error_not_a_default() {
        // Every default behind these parsers is more expensive than the request the client
        // meant: a full config read, a full case read, a default page size. Discarding the
        // error with `.ok().flatten()` turns "you sent nonsense" into "you sent nothing",
        // which is why the route layer takes the `Result` and never an `Option`.
        for q in [
            "?since_rev=twelve",
            "?since_rev=1.5",
            "?since_rev=-",
            "?since_rev=9x",
        ] {
            let err = parse_since_rev(q).unwrap_err();
            assert_eq!(err.http_status(), 422, "{q} must be a client error");
        }
        assert_eq!(
            parse_i64("?limit=lots", "limit").unwrap_err().http_status(),
            422
        );
        assert_eq!(
            parse_u32("?limit=lots", "limit").unwrap_err().http_status(),
            422
        );

        // Out of range is a client error too, not a silent wrap.
        assert_eq!(
            parse_u32("?limit=-1", "limit").unwrap_err().http_status(),
            422
        );

        // Absent and empty both mean "not specified", which is a legitimate default.
        assert_eq!(parse_since_rev("").unwrap(), None);
        assert_eq!(parse_since_rev("?other=1").unwrap(), None);
        assert_eq!(parse_i64("?limit=", "limit").unwrap(), None);
    }

    #[test]
    fn a_storage_error_never_leaks_its_detail() {
        let r = error(LogicError::Storage(
            "SELECT value_text FROM field_value".into(),
        ));
        assert_eq!(r.status, 500);
        let body = r.body.unwrap();
        assert!(!body.contains("value_text"), "query text leaked: {body}");
    }

    #[test]
    fn bodyless_statuses_carry_no_body() {
        assert_eq!(no_content(), Rendered::empty(204));
        assert_eq!(not_modified(), Rendered::empty(304));
    }

    #[test]
    fn query_parsing_handles_the_documented_parameters() {
        let q = "?assignee=me&since=2026-08-17T00%3A00%3A00Z&limit=25";
        assert_eq!(query_value(q, "assignee").as_deref(), Some("me"));
        assert_eq!(
            query_value(q, "since").as_deref(),
            Some("2026-08-17T00:00:00Z")
        );
        assert_eq!(parse_u32(q, "limit").unwrap(), Some(25));
        assert_eq!(query_value(q, "missing"), None);
    }

    #[test]
    fn since_rev_is_optional_but_never_silently_ignored() {
        assert_eq!(parse_since_rev("").unwrap(), None);
        assert_eq!(parse_since_rev("?since_rev=12").unwrap(), Some(CaseRev(12)));
        assert!(
            parse_since_rev("?since_rev=twelve").is_err(),
            "a bad since_rev must not fall back to a full read"
        );
    }
}
