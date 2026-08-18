//! Store behaviour tests.
//!
//! These cover the guarantees the rest of the system is built on: atomicity of the write
//! path, outbox coalescing, refusal to clobber unsynced work, and the physical layout that
//! makes R13 achievable.

use medatat_core::def::{FieldDef, FieldKind, FieldOption, SectionDef, SectionField};
use medatat_core::wire::{CaseSummary, ValueRow};
use medatat_core::{
    CaseId, CaseRev, ConfigRev, FieldId, FieldIdx, FormDef, FormId, OptionCode, SectionId, Value,
};
use medatat_core::{parse_date, parse_decimal, parse_time_24};
use medatat_store::{Store, StoreError};
use std::sync::Arc;

// ------------------------------------------------------------------ fixtures

fn all_kinds() -> Vec<FieldKind> {
    let opts: Arc<[FieldOption]> = Arc::from(vec![
        FieldOption {
            code: OptionCode::new("M"),
            label: "Male".into(),
            ordinal: 0,
        },
        FieldOption {
            code: OptionCode::new("F"),
            label: "Female".into(),
            ordinal: 1,
        },
    ]);
    vec![
        FieldKind::Text { max_len: Some(64) },
        FieldKind::Numeric {
            min: None,
            max: None,
            scale: 2,
        },
        FieldKind::Date,
        FieldKind::Time,
        FieldKind::Radio {
            options: opts.clone(),
        },
        FieldKind::Select {
            options: opts,
            searchable: true,
        },
        FieldKind::Textarea {
            rows: 4,
            max_len: None,
        },
    ]
}

/// A form with one field of every kind, laid out across 1-, 2-, and 3-column sections.
fn fixture_form() -> FormDef {
    let fields: Vec<SectionField> = all_kinds()
        .into_iter()
        .enumerate()
        .map(|(i, kind)| SectionField {
            idx: FieldIdx(0),
            field: Arc::new(FieldDef {
                field_id: FieldId::new(),
                key: format!("f{i}"),
                kind,
            }),
            label: format!("Field {i}"),
            ordinal: i as i32,
            col_span: 1,
            required: false,
        })
        .collect();

    FormDef::new(
        FormId::new(),
        "Fixture",
        vec![SectionDef {
            section_id: SectionId::new(),
            title: "All kinds".into(),
            ordinal: 0,
            columns: 3,
            default_collapsed: false,
            fields,
        }],
    )
}

/// Opens a file-backed store without touching the OS keychain.
///
/// Under `phi`, `Store::open` fetches the key from the platform keychain, which prompts —
/// and a freshly rebuilt test binary is a different binary to macOS, so the prompt cannot
/// be answered in CI. Tests supply their own key instead; the keychain path is exercised
/// by the application, and by checklist item 6 in `docs/12-PHI-READINESS.md`.
#[cfg(feature = "phi")]
fn open_file(path: &std::path::Path) -> Result<Store, StoreError> {
    Store::open_with_key(path, &"7f".repeat(32))
}

#[cfg(not(feature = "phi"))]
fn open_file(path: &std::path::Path) -> Result<Store, StoreError> {
    Store::open(path)
}

fn seeded() -> (Store, FormDef, CaseId) {
    let store = Store::open_in_memory().expect("open");
    let form = fixture_form();
    store.save_form(&form, ConfigRev(1)).expect("save form");

    let case_id = CaseId::new();
    store
        .upsert_case(&CaseSummary {
            case_id,
            mrn: "MRN-1".into(),
            form_id: form.form_id,
            assignee: Some("me".into()),
            rev: CaseRev::ZERO,
            updated_at: "2026-08-17T00:00:00Z".into(),
        })
        .expect("upsert case");
    (store, form, case_id)
}

fn ids(form: &FormDef) -> Vec<FieldId> {
    form.iter_fields().map(|f| f.field.field_id).collect()
}

// -------------------------------------------------------------------- schema

#[test]
fn schema_applies_and_reports_its_version() {
    let s = Store::open_in_memory().unwrap();
    assert_eq!(s.schema_version().unwrap(), 1);
}

#[test]
fn migrations_are_idempotent_across_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("m.db");
    assert_eq!(open_file(&path).unwrap().schema_version().unwrap(), 1);
    assert_eq!(open_file(&path).unwrap().schema_version().unwrap(), 1);
}

#[test]
fn form_definitions_round_trip() {
    let (store, form, _) = seeded();
    let loaded = store.load_form(form.form_id).unwrap();
    assert_eq!(loaded.name, form.name);
    assert_eq!(loaded.field_count(), form.field_count());
    // finalize() must have run on load, or FieldIdx lookups would be empty.
    for f in form.iter_fields() {
        assert_eq!(loaded.idx_of(f.field.field_id), Some(f.idx));
    }
    assert_eq!(store.load_all_forms().unwrap().len(), 1);
}

#[test]
fn missing_form_is_reported_not_panicked_on() {
    let s = Store::open_in_memory().unwrap();
    assert!(matches!(
        s.load_form(FormId::new()),
        Err(StoreError::FormNotFound(_))
    ));
}

// -------------------------------------------------------------- value fidelity

#[test]
fn r5_to_r11_every_kind_round_trips_exactly() {
    let (store, form, case_id) = seeded();
    let f = ids(&form);

    let values = vec![
        (f[0], Value::Text("Jane Q".into())),
        (f[1], Value::Num(parse_decimal("12.50").unwrap())),
        (f[2], Value::Date(parse_date("2026-08-17").unwrap())),
        (f[3], Value::Time(parse_time_24("09:05").unwrap())),
        (f[4], Value::Opt(OptionCode::new("M"))),
        (f[5], Value::Opt(OptionCode::new("F"))),
        (f[6], Value::Text("multi\nline".into())),
    ];
    store.apply_local(case_id, &values, CaseRev::ZERO).unwrap();

    let mut got = store.load_case_values(case_id).unwrap();
    got.sort_by_key(|(id, _)| f.iter().position(|x| x == id).unwrap());
    assert_eq!(
        got, values,
        "every kind must survive a save/load round trip"
    );
}

#[test]
fn r6_decimal_precision_survives_storage() {
    // The reason value_numeric is TEXT: REAL would round-trip 0.1 as 0.1000000000000000055.
    let (store, form, case_id) = seeded();
    let f = ids(&form);
    for s in ["0.1", "12.50", "999999.999", "-0.05", "0"] {
        let v = Value::Num(parse_decimal(s).unwrap());
        store
            .apply_local(case_id, &[(f[1], v.clone())], CaseRev::ZERO)
            .unwrap();
        assert_eq!(store.value(case_id, f[1]).unwrap(), Some(v), "decimal {s}");
    }
}

#[test]
fn text_and_option_are_distinguishable_after_reload() {
    // Both land in value_text; only the value_kind discriminant tells them apart.
    let (store, form, case_id) = seeded();
    let f = ids(&form);
    store
        .apply_local(
            case_id,
            &[
                (f[0], Value::Text("M".into())),
                (f[4], Value::Opt(OptionCode::new("M"))),
            ],
            CaseRev::ZERO,
        )
        .unwrap();

    assert_eq!(
        store.value(case_id, f[0]).unwrap(),
        Some(Value::Text("M".into()))
    );
    assert_eq!(
        store.value(case_id, f[4]).unwrap(),
        Some(Value::Opt(OptionCode::new("M")))
    );
}

#[test]
fn null_round_trips_as_null() {
    let (store, form, case_id) = seeded();
    let f = ids(&form);
    store
        .apply_local(case_id, &[(f[0], Value::Null)], CaseRev::ZERO)
        .unwrap();
    assert_eq!(store.value(case_id, f[0]).unwrap(), Some(Value::Null));
}

// ----------------------------------------------------------------- write path

#[test]
fn apply_local_writes_value_and_outbox_in_one_transaction() {
    let (store, form, case_id) = seeded();
    let f = ids(&form);
    store
        .apply_local(case_id, &[(f[0], Value::Text("x".into()))], CaseRev::ZERO)
        .unwrap();

    assert!(
        store.value(case_id, f[0]).unwrap().is_some(),
        "value must be saved"
    );
    let out = store.next_outbox_batch(10).unwrap();
    assert_eq!(
        out.len(),
        1,
        "and enqueued — a value saved but unqueued is silent data loss"
    );
    assert_eq!(out[0].field_id, f[0]);
}

#[test]
fn outbox_coalesces_repeated_edits_to_one_row() {
    let (store, form, case_id) = seeded();
    let f = ids(&form);
    for i in 0..40 {
        store
            .apply_local(
                case_id,
                &[(f[0], Value::Text(format!("keystroke {i}")))],
                CaseRev::ZERO,
            )
            .unwrap();
    }
    let out = store.next_outbox_batch(100).unwrap();
    assert_eq!(
        out.len(),
        1,
        "sync volume is bounded by fields touched, not keystrokes"
    );
    assert_eq!(
        out[0].value,
        Value::Text("keystroke 39".into()),
        "last write wins"
    );
}

#[test]
fn distinct_fields_get_distinct_outbox_rows() {
    let (store, form, case_id) = seeded();
    let f = ids(&form);
    store
        .apply_local(
            case_id,
            &[
                (f[0], Value::Text("a".into())),
                (f[6], Value::Text("b".into())),
            ],
            CaseRev::ZERO,
        )
        .unwrap();
    assert_eq!(store.next_outbox_batch(10).unwrap().len(), 2);
}

#[test]
fn confirm_clears_pending_and_drains_the_outbox() {
    let (store, form, case_id) = seeded();
    let f = ids(&form);
    store
        .apply_local(case_id, &[(f[0], Value::Text("x".into()))], CaseRev::ZERO)
        .unwrap();
    store.confirm(case_id, &[f[0]], CaseRev(1)).unwrap();

    assert!(store.next_outbox_batch(10).unwrap().is_empty());
    assert_eq!(store.synced_rev(case_id).unwrap(), Some(CaseRev(1)));
}

// ------------------------------------------------------------- inbound values

#[test]
fn apply_server_values_never_clobbers_an_unsynced_local_edit() {
    let (store, form, case_id) = seeded();
    let f = ids(&form);

    store
        .apply_local(
            case_id,
            &[(f[0], Value::Text("mine".into()))],
            CaseRev::ZERO,
        )
        .unwrap();
    store
        .apply_server_values(
            case_id,
            &[ValueRow {
                field_id: f[0],
                value: Value::Text("theirs".into()),
                rev: CaseRev(5),
                updated_by: None,
                updated_at: None,
            }],
            CaseRev(5),
        )
        .unwrap();

    assert_eq!(
        store.value(case_id, f[0]).unwrap(),
        Some(Value::Text("mine".into())),
        "an unsynced edit must survive an inbound sync"
    );
}

#[test]
fn apply_server_values_writes_fields_with_no_local_edit() {
    let (store, form, case_id) = seeded();
    let f = ids(&form);
    store
        .apply_server_values(
            case_id,
            &[ValueRow {
                field_id: f[3],
                value: Value::Time(parse_time_24("23:59").unwrap()),
                rev: CaseRev(2),
                updated_by: None,
                updated_at: None,
            }],
            CaseRev(2),
        )
        .unwrap();
    assert_eq!(
        store.value(case_id, f[3]).unwrap(),
        Some(Value::Time(parse_time_24("23:59").unwrap()))
    );
}

// ------------------------------------------------------------------ conflicts

#[test]
fn conflicts_are_recorded_and_survive_until_resolved() {
    let (store, form, case_id) = seeded();
    let f = ids(&form);
    store
        .apply_local(
            case_id,
            &[(f[0], Value::Text("mine".into()))],
            CaseRev::ZERO,
        )
        .unwrap();

    let theirs = ValueRow {
        field_id: f[0],
        value: Value::Text("theirs".into()),
        rev: CaseRev(7),
        updated_by: None,
        updated_at: None,
    };
    store.record_conflicts(case_id, &[theirs]).unwrap();

    let c = store.list_conflicts(case_id).unwrap();
    assert_eq!(c.len(), 1);
    assert_eq!(c[0].mine, Value::Text("mine".into()));
    assert_eq!(c[0].theirs, Value::Text("theirs".into()));
}

#[test]
fn resolving_take_theirs_clears_the_conflict_and_the_value() {
    let (store, form, case_id) = seeded();
    let f = ids(&form);
    store
        .apply_local(
            case_id,
            &[(f[0], Value::Text("mine".into()))],
            CaseRev::ZERO,
        )
        .unwrap();
    store
        .record_conflicts(
            case_id,
            &[ValueRow {
                field_id: f[0],
                value: Value::Text("theirs".into()),
                rev: CaseRev(7),
                updated_by: None,
                updated_at: None,
            }],
        )
        .unwrap();

    store.resolve_conflict(case_id, f[0], false).unwrap();
    assert!(store.list_conflicts(case_id).unwrap().is_empty());
    assert_eq!(
        store.value(case_id, f[0]).unwrap(),
        Some(Value::Text("theirs".into()))
    );
}

#[test]
fn resolving_keep_mine_re_enqueues_for_sync() {
    let (store, form, case_id) = seeded();
    let f = ids(&form);
    store
        .apply_local(
            case_id,
            &[(f[0], Value::Text("mine".into()))],
            CaseRev::ZERO,
        )
        .unwrap();
    store
        .record_conflicts(
            case_id,
            &[ValueRow {
                field_id: f[0],
                value: Value::Text("theirs".into()),
                rev: CaseRev(7),
                updated_by: None,
                updated_at: None,
            }],
        )
        .unwrap();
    store.drop_outbox(case_id, &[f[0]]).unwrap();

    store.resolve_conflict(case_id, f[0], true).unwrap();
    assert!(store.list_conflicts(case_id).unwrap().is_empty());
    assert_eq!(
        store.value(case_id, f[0]).unwrap(),
        Some(Value::Text("mine".into()))
    );
    assert_eq!(
        store.next_outbox_batch(10).unwrap().len(),
        1,
        "must be re-queued"
    );
}

// ------------------------------------------------------------------- worklist

#[test]
fn worklist_returns_only_the_users_cases_newest_first() {
    let store = Store::open_in_memory().unwrap();
    let form = fixture_form();
    store.save_form(&form, ConfigRev(1)).unwrap();

    for (i, who) in [("me", 1), ("you", 2), ("me", 3)]
        .iter()
        .map(|(w, i)| (*i, *w))
    {
        store
            .upsert_case(&CaseSummary {
                case_id: CaseId::new(),
                mrn: format!("MRN-{i}"),
                form_id: form.form_id,
                assignee: Some(who.into()),
                rev: CaseRev::ZERO,
                updated_at: format!("2026-08-1{i}T00:00:00Z"),
            })
            .unwrap();
    }

    let mine = store.worklist("me", 10).unwrap();
    assert_eq!(mine.len(), 2);
    assert!(mine[0].updated_at > mine[1].updated_at, "newest first");
}

// ----------------------------------------------------------------- sync state

#[test]
fn sync_state_round_trips_and_upserts() {
    let s = Store::open_in_memory().unwrap();
    assert_eq!(s.sync_state("config_rev").unwrap(), None);
    s.set_sync_state("config_rev", "41").unwrap();
    assert_eq!(s.sync_state("config_rev").unwrap(), Some("41".into()));
    s.set_sync_state("config_rev", "42").unwrap();
    assert_eq!(s.sync_state("config_rev").unwrap(), Some("42".into()));
}

// -------------------------------------------------------- the R13 layout guard

#[test]
fn case_load_uses_the_primary_key_not_a_table_scan() {
    // This protects the R13 latency margin. `field_value` is WITHOUT ROWID keyed on
    // (case_id, field_id), so one case's values are physically contiguous. If this
    // regresses to a scan, Bench 1 regresses with it — and this test says why.
    let (store, _, _) = seeded();
    let plan: Vec<String> = store
        .with_read(|c| {
            let mut stmt = c
                .prepare(
                    "EXPLAIN QUERY PLAN SELECT field_id, value_kind, value_text, \
                     value_numeric, value_date, value_time FROM field_value WHERE case_id = ?1",
                )
                .unwrap();
            stmt.query_map(["x"], |r| r.get::<_, String>(3))
                .unwrap()
                .filter_map(Result::ok)
                .collect()
        })
        .unwrap();
    let joined = plan.join(" | ");
    assert!(
        joined.contains("USING PRIMARY KEY") || joined.contains("USING INDEX"),
        "case load must not be a table scan, got: {joined}"
    );
    assert!(
        !joined.contains("SCAN field_value"),
        "full table scan: {joined}"
    );
}
