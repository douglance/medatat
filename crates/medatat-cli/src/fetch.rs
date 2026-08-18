//! `incurs` fetch gateway over HTTP.
//!
//! `incurs` is a CLI *framework*, not a test runner: it parses curl-style argv into a
//! [`FetchInput`] and renders a [`FetchOutput`]. Because the handler is a trait, the CLI
//! does not constrain the Worker's language — this implementation simply speaks HTTP to
//! `wrangler dev` or to the deployed Worker.
//!
//! Assertions live in `scripts/smoke.sh` (jq) and in `cargo test`; incurs provides no
//! assertion DSL.

use async_trait::async_trait;
use incurs::fetch::{FetchHandler, FetchInput, FetchOutput};
use serde_json::Value as Json;

pub struct HttpFetch {
    base: String,
    client: reqwest::Client,
    /// Applied to every request unless the caller passes their own Authorization header.
    default_token: Option<String>,
}

impl HttpFetch {
    pub fn new(base: impl Into<String>, default_token: Option<String>) -> Self {
        let base = base.into();
        HttpFetch {
            base: base.trim_end_matches('/').to_string(),
            client: reqwest::Client::new(),
            default_token,
        }
    }

    fn url(&self, input: &FetchInput) -> String {
        let mut url = format!("{}{}", self.base, input.path);
        if !input.query.is_empty() {
            let qs: Vec<String> = input
                .query
                .iter()
                .map(|(k, v)| format!("{}={}", urlencode(k), urlencode(v)))
                .collect();
            url.push('?');
            url.push_str(&qs.join("&"));
        }
        url
    }
}

#[async_trait]
impl FetchHandler for HttpFetch {
    async fn handle(&self, input: FetchInput) -> FetchOutput {
        let url = self.url(&input);
        let method =
            reqwest::Method::from_bytes(input.method.as_bytes()).unwrap_or(reqwest::Method::GET);

        let mut req = self.client.request(method, &url);
        let has_auth = input
            .headers
            .iter()
            .any(|(k, _)| k.eq_ignore_ascii_case("authorization"));
        for (k, v) in &input.headers {
            req = req.header(k, v);
        }
        if !has_auth && let Some(t) = &self.default_token {
            req = req.header("Authorization", format!("Bearer {t}"));
        }
        if let Some(body) = &input.body {
            req = req
                .header("Content-Type", "application/json")
                .body(body.clone());
        }

        match req.send().await {
            Ok(resp) => {
                let status = resp.status().as_u16();
                let headers = resp
                    .headers()
                    .iter()
                    .map(|(k, v)| (k.to_string(), v.to_str().unwrap_or_default().to_string()))
                    .collect();
                let text = resp.text().await.unwrap_or_default();
                let data =
                    serde_json::from_str::<Json>(&text).unwrap_or_else(|_| Json::String(text));
                FetchOutput {
                    ok: (200..300).contains(&status),
                    status,
                    data,
                    headers,
                }
            }
            // A transport failure is reported in the envelope rather than panicking, so
            // shell callers can branch on `.ok` uniformly.
            Err(e) => FetchOutput {
                ok: false,
                status: 0,
                data: serde_json::json!({
                    "ok": false,
                    "error": { "code": "internal", "message": e.to_string() }
                }),
                headers: vec![],
            },
        }
    }
}

fn urlencode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            b' ' => out.push_str("%20"),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn input(path: &str) -> FetchInput {
        FetchInput {
            path: path.into(),
            method: "GET".into(),
            headers: vec![],
            body: None,
            query: vec![],
        }
    }

    #[test]
    fn builds_url_without_double_slash() {
        let f = HttpFetch::new("http://localhost:8787/", None);
        assert_eq!(f.url(&input("/health")), "http://localhost:8787/health");
    }

    #[test]
    fn encodes_query_parameters() {
        let f = HttpFetch::new("http://x", None);
        let mut i = input("/cases");
        i.query = vec![("assignee".into(), "a b/c".into())];
        assert_eq!(f.url(&i), "http://x/cases?assignee=a%20b%2Fc");
    }

    #[test]
    fn urlencode_leaves_unreserved_alone() {
        assert_eq!(urlencode("abcXYZ019-_.~"), "abcXYZ019-_.~");
        assert_eq!(urlencode("a+b"), "a%2Bb");
    }
}
