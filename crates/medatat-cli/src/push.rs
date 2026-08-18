//! `medatat push` — get a synthetic corpus *into* a running Worker.
//!
//! `medatat seed` writes a corpus to disk; nothing consumed it, so Bench 4 (cold-DO read
//! against a seeded corpus) had no corpus to read and M1's scale criterion was unreachable.
//! This command closes that gap.
//!
//! **It writes through the ordinary API**, not a bulk path: `POST /cases` then repeated
//! `POST /cases/{id}/values`. That is slower than a fan-out endpoint and deliberately so.
//! A bulk write that stamped a whole case in one shot would give every row the same `rev`,
//! and both per-field conflict detection and `since_rev` delta reads key off the spread of
//! revs across a case. A corpus where every row is `rev 1` would make delta sync
//! unbenchmarkable *and* flatter the numbers.
//!
//! **Synthetic data only.** Everything comes from `medatat-testkit`; nothing here reads a
//! clinical source. See `docs/12-PHI-READINESS.md`.

use anyhow::{Context, Result, anyhow, bail};
use medatat_core::def::FormDef;
use medatat_core::ids::{CaseRev, FieldId};
use medatat_core::value::Value;
use medatat_core::wire::{PutValuesReq, ValueChange};
use serde::Deserialize;
use serde::de::DeserializeOwned;
use std::collections::HashMap;
use std::time::{Duration, Instant};

/// Values per `POST /cases/{id}/values`. A real abstractor's case accumulates over many
/// saves, so the corpus is written the same way — see the module docs on rev spread.
pub const DEFAULT_BATCH: usize = 50;
pub const DEFAULT_CASES: usize = 1_000;
pub const DEFAULT_FIELDS: usize = 1_000;
/// The full corpus M7 would commit to. Every run extrapolates to it.
pub const FULL_CORPUS: usize = 100_000;

// ---------------------------------------------------------------- pure planning

/// Split one case's values into the batches it will be written in.
///
/// Batching is the whole point: `n` batches produce revs `1..=n` with each row carrying the
/// rev it was written at, which is the shape real abstraction produces and the shape delta
/// sync has to be benchmarked against.
pub fn plan_batches(values: Vec<(FieldId, Value)>, batch: usize) -> Vec<Vec<ValueChange>> {
    let batch = batch.max(1);
    values
        .chunks(batch)
        .map(|chunk| {
            chunk
                .iter()
                .map(|(field_id, value)| ValueChange {
                    field_id: *field_id,
                    value: value.clone(),
                })
                .collect()
        })
        .collect()
}

/// `field.key` and `form.key` are UNIQUE in D1, and the generator's keys are a pure
/// function of the field's position — `f0000_text` every time. Without a per-run namespace
/// the second seeding run against the same Worker dies on a constraint violation.
pub fn namespaced_key(label: &str, key: &str) -> String {
    format!("{label}-{key}")
}

/// A label unique to this run, used to namespace every key it creates.
pub fn run_label(seed: u64) -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    format!("seed{seed:x}t{secs:x}")
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PushStats {
    pub cases: usize,
    pub values: usize,
    pub batches: usize,
    /// Every HTTP call, including form publication and case creation. This is what the
    /// wall clock is actually paying for.
    pub requests: usize,
    pub elapsed: Duration,
}

impl PushStats {
    pub fn cases_per_sec(&self) -> f64 {
        let s = self.elapsed.as_secs_f64();
        if s > 0.0 { self.cases as f64 / s } else { 0.0 }
    }

    /// How long the same rate would take for `target` cases. If this says days, that is a
    /// finding, not an inconvenience.
    pub fn extrapolate(&self, target: usize) -> Duration {
        if self.cases == 0 {
            return Duration::ZERO;
        }
        self.elapsed.mul_f64(target as f64 / self.cases as f64)
    }
}

/// The operator should not have to do the arithmetic, so the projection is printed rather
/// than left implicit — same contract as `medatat seed`.
pub fn render_report(stats: &PushStats, target: usize) -> String {
    let mut out = format!(
        "pushed {} cases x {} values = {} values in {} batches ({} requests) in {:.1}s ({:.1} cases/s)",
        stats.cases,
        stats.values.checked_div(stats.cases).unwrap_or(0),
        stats.values,
        stats.batches,
        stats.requests,
        stats.elapsed.as_secs_f64(),
        stats.cases_per_sec(),
    );
    if stats.cases > 0 && stats.cases < target {
        let projected = stats.extrapolate(target);
        out.push_str(&format!(
            "\nextrapolated to {} cases: ~{:.0} min ({:.1} h) and ~{} values",
            target,
            projected.as_secs_f64() / 60.0,
            projected.as_secs_f64() / 3600.0,
            stats.values * target / stats.cases.max(1),
        ));
    }
    out
}

// ------------------------------------------------------------------- HTTP client

struct Api {
    base: String,
    token: String,
    client: reqwest::Client,
    requests: std::cell::Cell<usize>,
}

#[derive(Deserialize)]
struct Envelope<T> {
    #[allow(dead_code)]
    ok: bool,
    data: Option<T>,
    error: Option<serde_json::Value>,
}

#[derive(Deserialize)]
struct CreatedForm {
    form_id: medatat_core::ids::FormId,
}
#[derive(Deserialize)]
struct CreatedSection {
    section_id: medatat_core::ids::SectionId,
}
#[derive(Deserialize)]
struct CreatedField {
    field_id: FieldId,
}
#[derive(Deserialize)]
struct CreatedCase {
    case_id: medatat_core::ids::CaseId,
}
#[derive(Deserialize)]
struct Applied {
    rev: CaseRev,
}

impl Api {
    fn new(base: &str, token: &str) -> Self {
        Api {
            base: base.trim_end_matches('/').to_string(),
            token: token.to_string(),
            client: reqwest::Client::new(),
            requests: std::cell::Cell::new(0),
        }
    }

    async fn post<B: serde::Serialize, T: DeserializeOwned>(
        &self,
        path: &str,
        body: &B,
    ) -> Result<T> {
        self.requests.set(self.requests.get() + 1);
        let resp = self
            .client
            .post(format!("{}{path}", self.base))
            .header("Authorization", format!("Bearer {}", self.token))
            .json(body)
            .send()
            .await
            .with_context(|| format!("POST {path}"))?;

        let status = resp.status().as_u16();
        let text = resp.text().await.unwrap_or_default();

        // 204 carries no body; anything else should be an envelope.
        if status == 204 {
            return serde_json::from_str("null")
                .map_err(|_| anyhow!("POST {path} returned 204 but a body was expected"));
        }
        let env: Envelope<T> = serde_json::from_str(&text)
            .with_context(|| format!("POST {path} -> {status}, unparseable body: {text}"))?;
        if let Some(err) = env.error {
            bail!("POST {path} -> {status}: {err}");
        }
        env.data
            .ok_or_else(|| anyhow!("POST {path} -> {status}: envelope carried no data"))
    }

    /// For endpoints that answer 204.
    async fn post_no_content<B: serde::Serialize>(&self, path: &str, body: &B) -> Result<()> {
        self.requests.set(self.requests.get() + 1);
        let resp = self
            .client
            .post(format!("{}{path}", self.base))
            .header("Authorization", format!("Bearer {}", self.token))
            .json(body)
            .send()
            .await
            .with_context(|| format!("POST {path}"))?;
        let status = resp.status().as_u16();
        if (200..300).contains(&status) {
            return Ok(());
        }
        let text = resp.text().await.unwrap_or_default();
        bail!("POST {path} -> {status}: {text}")
    }
}

/// What the Worker assigned when the synthetic form was published.
pub struct Published {
    pub form_id: medatat_core::ids::FormId,
    /// Local generated `FieldId` -> the id D1 assigned. Values must be remapped through
    /// this or every change is a 404: the Worker validates against *its* configuration.
    pub field_map: HashMap<FieldId, FieldId>,
}

/// Create the form, its sections, its fields, and their placements.
///
/// This step is not optional and is easy to miss: a form generated in this process exists
/// nowhere on the server, and `POST /cases/{id}/values` re-validates every value against
/// the configuration in D1. Without publishing first, every single value is a 404.
async fn publish_form(api: &Api, form: &FormDef, label: &str) -> Result<Published> {
    let created: CreatedForm = api
        .post(
            "/config/forms",
            &serde_json::json!({
                "key": namespaced_key(label, "corpus"),
                "name": form.name,
            }),
        )
        .await
        .context("creating the form (needs an admin token)")?;

    let mut field_map = HashMap::new();

    for section in &form.sections {
        let sec: CreatedSection = api
            .post(
                &format!("/config/forms/{}/sections", created.form_id),
                &serde_json::json!({
                    "name": section.title,
                    "ordinal": section.ordinal,
                    "columns": section.columns,
                }),
            )
            .await
            .with_context(|| format!("creating section {}", section.title))?;

        for placement in &section.fields {
            // `CreateFieldReq` flattens `FieldKind`, so the kind tag and its config land as
            // sibling keys — the shape `docs/03-API.md` documents.
            let mut body =
                serde_json::to_value(&placement.field.kind).context("serialising a field kind")?;
            body.as_object_mut()
                .ok_or_else(|| anyhow!("field kind did not serialise to an object"))?
                .insert(
                    "key".into(),
                    serde_json::Value::String(namespaced_key(label, &placement.field.key)),
                );

            let field: CreatedField = api
                .post("/config/fields", &body)
                .await
                .with_context(|| format!("creating field {}", placement.field.key))?;
            field_map.insert(placement.field.field_id, field.field_id);

            api.post_no_content(
                &format!("/config/sections/{}/fields", sec.section_id),
                &serde_json::json!({
                    "field_id": field.field_id,
                    "ordinal": placement.ordinal,
                    "col_span": placement.col_span,
                    "label": placement.label,
                    "required": placement.required,
                }),
            )
            .await
            .with_context(|| format!("placing field {}", placement.field.key))?;
        }
    }

    Ok(Published {
        form_id: created.form_id,
        field_map,
    })
}

/// Create one case and write its values in batches, tracking `rev` the way a real client
/// does. Returns the number of values written.
async fn push_case(
    api: &Api,
    published: &Published,
    mrn: &str,
    values: Vec<(FieldId, Value)>,
    batch: usize,
) -> Result<(usize, usize)> {
    let case: CreatedCase = api
        .post(
            "/cases",
            &serde_json::json!({
                "mrn": mrn,
                "form_id": published.form_id,
                "assignee": serde_json::Value::Null,
            }),
        )
        .await
        .context("creating a case")?;

    // Remap to the ids the server assigned. A miss here means the form published a
    // different field set than the one generating values, which is a bug, not bad input.
    let remapped: Vec<(FieldId, Value)> = values
        .into_iter()
        .map(|(id, v)| {
            published
                .field_map
                .get(&id)
                .copied()
                .map(|server_id| (server_id, v))
                .ok_or_else(|| anyhow!("generated field {id} was never published"))
        })
        .collect::<Result<_>>()?;

    let batches = plan_batches(remapped, batch);
    let mut written = 0usize;
    let mut base_rev = CaseRev::ZERO;

    for changes in &batches {
        written += changes.len();
        let applied: Applied = api
            .post(
                &format!("/cases/{}/values", case.case_id),
                &PutValuesReq {
                    base_rev,
                    changes: changes.clone(),
                },
            )
            .await
            .with_context(|| format!("writing values to case {}", case.case_id))?;
        // Track the server's rev, as a real client does after an accepted batch.
        base_rev = applied.rev;
    }

    Ok((written, batches.len()))
}

// ------------------------------------------------------------------------ command

pub async fn run(args: &[String]) -> Result<()> {
    let mut cases = DEFAULT_CASES;
    let mut fields = DEFAULT_FIELDS;
    let mut batch = DEFAULT_BATCH;
    let mut seed = medatat_testkit::DEFAULT_SEED;
    let mut start = 0usize;
    // 8 is deliberately modest. Cases are independent Durable Objects so they parallelise
    // cleanly, but each is ~74 requests and the point is to fill the latency gap, not to
    // find the server's breaking point.
    let mut concurrency = 8usize;
    let mut reuse_form: Option<String> = None;
    let mut api_base =
        std::env::var("MEDATAT_API").unwrap_or_else(|_| "http://localhost:8787".to_string());

    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--cases" => cases = next_num(&mut it, "--cases")? as usize,
            "--fields" => fields = next_num(&mut it, "--fields")? as usize,
            "--batch" => batch = next_num(&mut it, "--batch")? as usize,
            "--concurrency" => {
                concurrency = (next_num(&mut it, "--concurrency")? as usize).clamp(1, 64)
            }
            "--seed" => seed = next_num(&mut it, "--seed")?,
            // Resume. The local Worker emulator dies of V8 heap exhaustion after a few
            // hundred live Durable Objects, so a long run has to be driven in chunks with
            // a restart between them. Case seeds derive from the index, so resuming at N
            // produces exactly the cases a single uninterrupted run would have.
            "--start" => start = next_num(&mut it, "--start")? as usize,
            // Reuse a form published by an earlier chunk instead of publishing another.
            "--form-label" => {
                reuse_form = Some(
                    it.next()
                        .ok_or_else(|| anyhow!("--form-label needs a label"))?
                        .to_string(),
                )
            }
            "--api" => {
                api_base = it
                    .next()
                    .ok_or_else(|| anyhow!("--api needs a URL"))?
                    .to_string()
            }
            "-h" | "--help" => {
                println!("{}", HELP);
                return Ok(());
            }
            other => bail!("unknown flag {other}"),
        }
    }

    let token = std::env::var("MEDATAT_TOKEN").map_err(|_| {
        anyhow!(
            "MEDATAT_TOKEN is not set. Publishing a form requires an admin session token \
             (POST /auth/request then /auth/verify)."
        )
    })?;

    let api = Api::new(&api_base, &token);
    let label = reuse_form.clone().unwrap_or_else(|| run_label(seed));
    let form = medatat_testkit::synthetic_form_seeded(fields, seed);

    eprintln!(
        "publishing a {}-field form as `{}` to {api_base} ...",
        form.field_count(),
        label
    );
    let started = Instant::now();
    let published = publish_form(&api, &form, &label).await?;
    eprintln!(
        "form published as {} in {:.1}s; pushing {cases} cases ...",
        published.form_id,
        started.elapsed().as_secs_f64()
    );

    let mut values_written = 0usize;
    let mut batches_written = 0usize;

    // Cases in flight at once.
    //
    // Serial pushing is dominated by round-trip latency, not by the server: a case is ~74
    // requests, so against a deployed Worker it measured 0.12 cases/s and 100k cases
    // extrapolated to 229 hours. That is not an optimisation problem, it is the difference
    // between a corpus being buildable and not.
    //
    // Concurrency lives here rather than in a bulk server endpoint on purpose. A bulk write
    // would stamp every row of a case with one rev, and per-field conflict detection and
    // `since_rev` delta reads both key off that spread — a corpus where every row is rev 1
    // makes delta sync unbenchmarkable *and* flatters the numbers.
    //
    // Cases are independent (one Durable Object each), so they parallelise cleanly; the
    // batches *within* a case stay strictly ordered, because that ordering is what produces
    // the rev spread.
    let mut in_flight = futures::stream::FuturesUnordered::new();
    let mut next = start;
    let mut done = 0usize;

    loop {
        while in_flight.len() < concurrency && next < cases {
            let i = next;
            next += 1;
            // Same derivation `seed_corpus` uses, so a disk corpus and a pushed corpus are
            // the same data and Bench 4 can be run against either. Deriving from the index
            // rather than a running counter is what keeps a concurrent run identical to a
            // serial one.
            let case_seed = seed ^ (i as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15);
            let values = medatat_testkit::synthetic_case(&form, case_seed);
            let mrn = medatat_testkit::synthetic_mrn(case_seed);
            let api = &api;
            let published = &published;
            in_flight.push(async move {
                push_case(api, published, &mrn, values, batch)
                    .await
                    .with_context(|| format!("case {i} of {cases}"))
            });
        }
        if in_flight.is_empty() {
            break;
        }

        use futures::StreamExt as _;
        let (written, batches) = in_flight.next().await.expect("in_flight is non-empty")?;
        values_written += written;
        batches_written += batches;
        done += 1;

        let total = cases - start;
        if total >= 20 && done % (total / 20).max(1) == 0 {
            eprintln!(
                "  {}/{cases} cases ({:.0}s)",
                start + done,
                started.elapsed().as_secs_f64()
            );
        }
    }

    let stats = PushStats {
        cases: cases - start,
        values: values_written,
        batches: batches_written,
        requests: api.requests.get(),
        elapsed: started.elapsed(),
    };
    println!("{}", render_report(&stats, FULL_CORPUS));
    Ok(())
}

const HELP: &str = "\
medatat push --cases <n> --fields <n> --batch <n> --seed <n> --concurrency <n> --api <url>

Pushes a synthetic corpus into a running Worker through the ordinary write path:
POST /cases, then repeated POST /cases/{id}/values. Slower than a bulk endpoint and
deliberately so — batching is what gives each case a realistic spread of revs, which is
what per-field conflict detection and since_rev delta reads are benchmarked against.

Publishes the generated form to /config first (admin token required): the Worker
re-validates every value against ITS configuration, so an unpublished form 404s.

  MEDATAT_API    base URL (default http://localhost:8787)
  MEDATAT_TOKEN  admin session token, required

Long runs against `wrangler dev` have to be chunked. The local emulator keeps every
Durable Object resident in one workerd process and aborts on V8 heap exhaustion after a
few hundred live objects; that ceiling is workerd's, not Node's, so NODE_OPTIONS does not
move it. Restart the Worker between chunks and resume:

  medatat push --cases 200 --fields 1000
  medatat push --cases 400 --start 200 --fields 1000 --form-label <label from chunk 1>

Case seeds derive from the index, so resuming at N yields exactly the cases an
uninterrupted run would have produced.

Run the 1,000-case extrapolation before committing to the full 100,000-case corpus
(docs/08-MILESTONES.md M1). Synthetic data only.";

fn next_num<'a>(it: &mut impl Iterator<Item = &'a String>, flag: &str) -> Result<u64> {
    it.next()
        .ok_or_else(|| anyhow!("{flag} needs a number"))?
        .parse()
        .map_err(|_| anyhow!("{flag} must be a number"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use medatat_core::ids::FieldId;

    fn vals(n: usize) -> Vec<(FieldId, Value)> {
        (0..n)
            .map(|i| (FieldId::new(), Value::Text(format!("v{i}"))))
            .collect()
    }

    #[test]
    fn batching_is_what_produces_a_realistic_rev_spread() {
        // 1,000 values in one batch would be one rev for the whole case — the exact defect
        // that ruled out a bulk endpoint. 50 at a time gives the case 20 revs.
        let batches = plan_batches(vals(1_000), 50);
        assert_eq!(batches.len(), 20);
        assert!(batches.iter().all(|b| b.len() == 50));
    }

    #[test]
    fn batching_preserves_every_value_exactly_once() {
        let values = vals(137);
        let ids: Vec<FieldId> = values.iter().map(|(id, _)| *id).collect();
        let batches = plan_batches(values, 20);

        assert_eq!(batches.len(), 7, "137 = 6 full batches + a remainder");
        assert_eq!(batches.last().unwrap().len(), 17);

        let flattened: Vec<FieldId> = batches
            .iter()
            .flat_map(|b| b.iter().map(|c| c.field_id))
            .collect();
        assert_eq!(flattened, ids, "order and membership must survive batching");
    }

    #[test]
    fn a_zero_batch_size_does_not_hang_or_divide_by_zero() {
        let batches = plan_batches(vals(3), 0);
        assert_eq!(
            batches.len(),
            3,
            "clamped to 1 per batch, not an infinite loop"
        );
    }

    #[test]
    fn an_empty_case_produces_no_requests() {
        assert!(plan_batches(vec![], 50).is_empty());
    }

    #[test]
    fn keys_are_namespaced_so_a_second_run_does_not_collide() {
        // field.key and form.key are UNIQUE in D1, and the generator's keys are fixed.
        let a = run_label(1);
        assert_ne!(
            namespaced_key(&a, "f0000_text"),
            namespaced_key("other", "f0000_text")
        );
        assert!(namespaced_key(&a, "f0000_text").ends_with("f0000_text"));
    }

    #[test]
    fn the_report_prints_the_projection_so_the_operator_does_not_do_the_arithmetic() {
        let stats = PushStats {
            cases: 1_000,
            values: 1_000_000,
            batches: 20_000,
            requests: 21_000,
            elapsed: Duration::from_secs(600),
        };
        assert_eq!(stats.cases_per_sec(), 1_000.0 / 600.0);
        assert_eq!(stats.extrapolate(100_000), Duration::from_secs(60_000));

        let report = render_report(&stats, 100_000);
        assert!(report.contains("1000 cases"), "{report}");
        assert!(report.contains("extrapolated to 100000 cases"), "{report}");
        assert!(report.contains("1000 min"), "{report}");
        assert!(report.contains("16.7 h"), "{report}");
    }

    #[test]
    fn a_full_size_run_reports_no_projection() {
        let stats = PushStats {
            cases: 100_000,
            values: 100,
            batches: 1,
            requests: 1,
            elapsed: Duration::from_secs(1),
        };
        assert!(!render_report(&stats, 100_000).contains("extrapolated"));
    }

    #[test]
    fn an_empty_run_does_not_divide_by_zero() {
        let stats = PushStats {
            cases: 0,
            values: 0,
            batches: 0,
            requests: 0,
            elapsed: Duration::from_secs(1),
        };
        assert_eq!(stats.extrapolate(100), Duration::ZERO);
        assert_eq!(stats.cases_per_sec(), 0.0);
        assert!(!render_report(&stats, 100_000).contains("extrapolated"));
    }
}
