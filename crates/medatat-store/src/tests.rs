//! In-crate tests. Requirement IDs are in the names, per AGENTS.md.

use super::*;
use medatat_core::{
    ActorId, FieldDef, FieldIdx, FieldKind, FieldOption, OptionCode, SectionDef, SectionField,
    SectionId, parse_date, parse_decimal, parse_time_24,
};

// ------------------------------------------------------------------ fixtures

/// One field of every kind, in a fixed order: text, numeric, date, time, radio, select,
/// textarea.
fn seven_kinds() -> Vec<FieldKind> {
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
            scale: 3,
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

fn seven_kind_form() -> FormDef {
    let fields = seven_kinds()
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
        "seven kinds",
        vec![SectionDef {
            section_id: SectionId::new(),
            title: "All kinds".into(),
            ordinal: 0,
            columns: 2,
            default_collapsed: false,
            fields,
        }],
    )
}

fn ids(def: &FormDef) -> Vec<FieldId> {
    def.iter_fields().map(|f| f.field.field_id).collect()
}

fn summary(case_id: CaseId, form_id: FormId, updated_at: &str) -> CaseSummary {
    CaseSummary {
        case_id,
        mrn: "MRN-0001".into(),
        form_id,
        assignee: Some("abstractor@example.test".into()),
        rev: CaseRev(0),
        updated_at: updated_at.into(),
    }
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

/// An in-memory store holding one form and one case.
fn fixture() -> (Store, FormDef, CaseId) {
    let store = Store::open_in_memory().expect("open_in_memory");
    let def = seven_kind_form();
    store.save_form(&def, ConfigRev(1)).expect("save_form");
    let case_id = CaseId::new();
    store
        .upsert_case(&summary(case_id, def.form_id, "2026-08-17T09:00:00.000Z"))
        .expect("upsert_case");
    (store, def, case_id)
}

fn value_map(rows: Vec<(FieldId, Value)>) -> std::collections::HashMap<FieldId, Value> {
    rows.into_iter().collect()
}

// ------------------------------------------------------------------ schema

#[test]
fn schema_applies_and_version_round_trips() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("medatat.db");

    let store = open_file(&path).expect("open");
    assert_eq!(store.schema_version().expect("version"), migrations::LATEST);

    let def = seven_kind_form();
    store.save_form(&def, ConfigRev(7)).expect("save_form");
    drop(store);

    // Reopening an existing database must migrate idempotently and keep its data.
    let store = open_file(&path).expect("reopen");
    assert_eq!(store.schema_version().expect("version"), migrations::LATEST);
    assert_eq!(store.load_all_forms().expect("forms").len(), 1);
    let loaded = store.load_form(def.form_id).expect("load_form");
    assert_eq!(loaded.field_count(), 7, "finalize must run after decode");
    assert_eq!(ids(&loaded), ids(&def));
}

/// The clean-machine case: `~/Library/Application Support/medatat/` does not exist yet.
/// SQLite creates the file but never the directory, and the app's response to a failed
/// open is an in-memory fallback — so this failing looks like a working app that loses
/// everything on quit. Every other test hides it by opening under a `tempdir` that
/// already exists.
#[test]
fn open_creates_the_directory_it_needs() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("nested/deeper/medatat.db");
    assert!(!path.exists());

    let store = open_file(&path).expect("open must create its own parent directories");
    assert!(path.exists(), "the database file must be on disk");

    let def = seven_kind_form();
    store.save_form(&def, ConfigRev(1)).expect("save_form");
    let case_id = CaseId::new();
    store
        .upsert_case(&summary(case_id, def.form_id, "2026-08-17T09:00:00.000Z"))
        .expect("upsert_case");
    let f = ids(&def)[0];
    store
        .apply_local(case_id, &[(f, Value::Text("persisted".into()))], CaseRev(0))
        .expect("apply_local");
    drop(store);

    // Reopened from disk: the value was really written, not held in memory.
    let store = open_file(&path).expect("reopen");
    assert_eq!(
        value_map(store.load_case_values(case_id).expect("load")).get(&f),
        Some(&Value::Text("persisted".into()))
    );
}

// ------------------------------------------------------------------ fields

/// The regression this table exists to prevent.
///
/// Unplacing a field removes only the placement. If the client tracked fields solely
/// inside `form.def_blob`, rewriting the form without it would erase the client's last
/// reference to a field whose values are still sitting in `field_value` — and the
/// builder's "Unplaced fields" drawer would come back empty after a restart
/// (`docs/06-FORM-BUILDER.md` acceptance item 9).
#[test]
fn a_field_placed_in_no_form_is_still_returned() {
    let (store, def, case_id) = fixture();
    let f = ids(&def)[0];
    let all: Vec<FieldDef> = def.iter_fields().map(|sf| (*sf.field).clone()).collect();
    store.save_fields(&all).expect("save_fields");
    store
        .apply_local(case_id, &[(f, Value::Text("kept".into()))], CaseRev(0))
        .expect("apply_local");

    // The coordinator removes every placement: same form_id, no sections at all.
    let emptied = FormDef::new(def.form_id, def.name.clone(), vec![]);
    store.save_form(&emptied, ConfigRev(2)).expect("save_form");
    assert_eq!(
        store
            .load_form(def.form_id)
            .expect("load_form")
            .field_count(),
        0,
        "the placements really are gone"
    );

    let fields = store.all_fields().expect("all_fields");
    assert_eq!(fields.len(), 7, "unplacing must never drop a field row");
    assert!(fields.iter().any(|d| d.field_id == f));
    assert_eq!(
        value_map(store.load_case_values(case_id).expect("load")).get(&f),
        Some(&Value::Text("kept".into())),
        "and the values it points at are still there"
    );
}

#[test]
fn fields_round_trip_in_key_order() {
    let (store, def, _) = fixture();
    let mut all: Vec<FieldDef> = def.iter_fields().map(|sf| (*sf.field).clone()).collect();
    // Saved out of order; `all_fields` is what imposes the drawer's ordering.
    all.reverse();
    store.save_fields(&all).expect("save_fields");

    let got = store.all_fields().expect("all_fields");
    let keys: Vec<&str> = got.iter().map(|d| d.key.as_str()).collect();
    assert_eq!(keys, ["f0", "f1", "f2", "f3", "f4", "f5", "f6"]);

    // Every kind survives the blob, including the option lists.
    let mut expect: Vec<FieldDef> = all;
    expect.sort_by(|a, b| a.key.cmp(&b.key));
    assert_eq!(got, expect);
}

#[test]
fn save_fields_upserts_rather_than_duplicating() {
    let (store, def, _) = fixture();
    let mut first = (*def.iter_fields().next().expect("a field").field).clone();
    store.save_fields(&[first.clone()]).expect("save_fields");

    first.key = "renamed".into();
    store.save_fields(&[first.clone()]).expect("save_fields");

    let got = store.all_fields().expect("all_fields");
    assert_eq!(got.len(), 1, "same field_id must update, not insert again");
    assert_eq!(got[0].key, "renamed");
}

#[test]
fn empty_save_fields_is_a_no_op() {
    let (store, _, _) = fixture();
    store.save_fields(&[]).expect("save_fields");
    assert!(store.all_fields().expect("all_fields").is_empty());
}

/// A database written before the `field` table existed must gain it on open, not be
/// rejected or rebuilt.
#[test]
fn a_v1_database_migrates_forward_to_v2() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("medatat.db");

    let store = open_file(&path).expect("open");
    let def = seven_kind_form();
    store.save_form(&def, ConfigRev(1)).expect("save_form");
    // Rewind to what a v1 database on disk looks like.
    store
        .exec("DROP TABLE field; DROP INDEX IF EXISTS field_key; DELETE FROM schema_version; INSERT INTO schema_version (version) VALUES (1)")
        .expect("rewind to v1");
    drop(store);

    let store = open_file(&path).expect("reopen must migrate, not fail");
    assert_eq!(store.schema_version().expect("version"), 2);
    assert!(
        store.all_fields().expect("all_fields").is_empty(),
        "the new table starts empty and fills from the next config sync"
    );
    assert_eq!(
        store.load_all_forms().expect("forms").len(),
        1,
        "the migration must not disturb existing data"
    );

    let all: Vec<FieldDef> = def.iter_fields().map(|sf| (*sf.field).clone()).collect();
    store.save_fields(&all).expect("save_fields");
    assert_eq!(store.all_fields().expect("all_fields").len(), 7);
}

#[test]
fn hot_tables_are_without_rowid() {
    let (store, _, _) = fixture();
    for table in ["field_value", "outbox", "conflict"] {
        let sql: String = {
            let conn = store.reader().expect("reader");
            conn.query_row(
                "SELECT sql FROM sqlite_schema WHERE type = 'table' AND name = ?1",
                params![table],
                |r| r.get(0),
            )
            .expect("schema sql")
        };
        assert!(
            sql.contains("WITHOUT ROWID"),
            "{table} must be WITHOUT ROWID:\n{sql}"
        );
    }
}

#[test]
fn missing_form_is_an_error_not_a_panic() {
    let (store, _, _) = fixture();
    assert!(matches!(
        store.load_form(FormId::new()),
        Err(StoreError::FormNotFound(_))
    ));
}

// ------------------------------------------------------------------ values

#[test]
fn r5_to_r11_every_kind_round_trips_exactly() {
    let (store, def, case_id) = fixture();
    let f = ids(&def);
    let expected = vec![
        (f[0], Value::Text("Jane Q".into())),
        (f[1], Value::Num(parse_decimal("12.500").expect("decimal"))),
        (f[2], Value::Date(parse_date("2024-02-29").expect("date"))),
        (f[3], Value::Time(parse_time_24("9:5").expect("time"))),
        (f[4], Value::Opt(OptionCode::new("F"))),
        (f[5], Value::Opt(OptionCode::new("M"))),
        (f[6], Value::Text("line one\nline two".into())),
    ];

    store
        .apply_local(case_id, &expected, CaseRev(0))
        .expect("apply_local");

    let got = value_map(store.load_case_values(case_id).expect("load"));
    assert_eq!(got.len(), 7);
    for (id, want) in &expected {
        assert_eq!(got.get(id), Some(want), "field {id} did not round-trip");
    }
}

#[test]
fn r6_numeric_is_text_not_real_and_keeps_its_scale() {
    let (store, def, case_id) = fixture();
    let f = ids(&def)[1];
    for s in ["12.50", "0.001", "999999.999", "-0.5", "0"] {
        let v = Value::Num(parse_decimal(s).expect("decimal"));
        store
            .apply_local(case_id, &[(f, v.clone())], CaseRev(0))
            .expect("apply_local");

        let stored: String = {
            let conn = store.reader().expect("reader");
            conn.query_row(
                "SELECT value_numeric FROM field_value WHERE case_id = ?1 AND field_id = ?2",
                params![case_id.to_string(), f.to_string()],
                |r| r.get(0),
            )
            .expect("value_numeric")
        };
        assert_eq!(
            stored, s,
            "decimal must be stored verbatim, not via a float"
        );

        let got = value_map(store.load_case_values(case_id).expect("load"));
        assert_eq!(got.get(&f), Some(&v));
    }

    // The declared column type is what stops a future writer binding an f64.
    let decl: String = {
        let conn = store.reader().expect("reader");
        conn.query_row(
            "SELECT type FROM pragma_table_info('field_value') WHERE name = 'value_numeric'",
            [],
            |r| r.get(0),
        )
        .expect("pragma_table_info")
    };
    assert_eq!(decl, "TEXT");
}

#[test]
fn r8_time_is_stored_zero_padded_24hr() {
    let (store, def, case_id) = fixture();
    let f = ids(&def)[3];
    for (typed, stored_as) in [("9:5", "09:05"), ("2359", "23:59"), ("0", "00:00")] {
        let v = Value::Time(parse_time_24(typed).expect("time"));
        store
            .apply_local(case_id, &[(f, v.clone())], CaseRev(0))
            .expect("apply_local");
        let stored: String = {
            let conn = store.reader().expect("reader");
            conn.query_row(
                "SELECT value_time FROM field_value WHERE case_id = ?1 AND field_id = ?2",
                params![case_id.to_string(), f.to_string()],
                |r| r.get(0),
            )
            .expect("value_time")
        };
        assert_eq!(stored, stored_as);
        assert_eq!(
            value_map(store.load_case_values(case_id).expect("load")).get(&f),
            Some(&v)
        );
    }
}

#[test]
fn text_and_option_are_distinguishable_after_a_reload() {
    // Both live in value_text; only the stored discriminant separates them.
    let (store, def, case_id) = fixture();
    let f = ids(&def);
    store
        .apply_local(
            case_id,
            &[
                (f[0], Value::Text("M".into())),
                (f[4], Value::Opt(OptionCode::new("M"))),
            ],
            CaseRev(0),
        )
        .expect("apply_local");
    let got = value_map(store.load_case_values(case_id).expect("load"));
    assert_eq!(got.get(&f[0]), Some(&Value::Text("M".into())));
    assert_eq!(got.get(&f[4]), Some(&Value::Opt(OptionCode::new("M"))));
}

#[test]
fn null_round_trips_as_null() {
    let (store, def, case_id) = fixture();
    let f = ids(&def)[0];
    store
        .apply_local(case_id, &[(f, Value::Text("x".into()))], CaseRev(0))
        .expect("apply_local");
    store
        .apply_local(case_id, &[(f, Value::Null)], CaseRev(0))
        .expect("clear");
    let got = value_map(store.load_case_values(case_id).expect("load"));
    assert_eq!(got.get(&f), Some(&Value::Null));
}

#[test]
fn r13_case_load_is_a_primary_key_range_scan() {
    let (store, _, _) = fixture();
    let plan = store.explain(values::LOAD_SQL).expect("explain");
    let joined = plan.join(" | ");
    assert!(
        joined.contains("SEARCH") && joined.contains("PRIMARY KEY"),
        "the R13 read must use the primary key, got: {joined}"
    );
    assert!(
        !joined.contains("SCAN field_value"),
        "the R13 read must not be a full table scan, got: {joined}"
    );
}

// ------------------------------------------------------------------ write path

#[test]
fn r14_forty_edits_to_one_field_coalesce_to_one_outbox_row() {
    let (store, def, case_id) = fixture();
    let f = ids(&def)[0];
    for i in 0..40 {
        store
            .apply_local(
                case_id,
                &[(f, Value::Text(format!("keystroke {i}")))],
                CaseRev(0),
            )
            .expect("apply_local");
    }

    assert_eq!(
        store
            .query_i64("SELECT count(*) FROM outbox")
            .expect("count"),
        1,
        "40 edits to one field must leave exactly one queued row"
    );
    let batch = store.next_outbox_batch(64).expect("batch");
    assert_eq!(batch.len(), 1);
    assert_eq!(batch[0].value, Value::Text("keystroke 39".into()));
    assert_eq!(batch[0].base_rev, CaseRev(0));
    assert_eq!(
        store
            .query_i64("SELECT count(*) FROM field_value")
            .expect("count"),
        1
    );
}

#[test]
fn r14_apply_local_writes_value_and_outbox_in_one_transaction() {
    let (store, def, case_id) = fixture();
    let f = ids(&def)[0];

    // Force the outbox insert to fail. If the two writes were not in one transaction the
    // value would survive un-enqueued, which is the exact failure this guards.
    store
        .exec(
            "CREATE TRIGGER fail_outbox BEFORE INSERT ON outbox \
             BEGIN SELECT RAISE(ABORT, 'simulated crash'); END",
        )
        .expect("trigger");

    let err = store
        .apply_local(case_id, &[(f, Value::Text("lost".into()))], CaseRev(0))
        .expect_err("the write must fail");
    assert!(matches!(err, StoreError::Sqlite(_)));

    assert_eq!(
        store
            .query_i64("SELECT count(*) FROM field_value")
            .expect("count"),
        0,
        "a value must never be saved without its outbox row"
    );
    assert_eq!(
        store
            .query_i64("SELECT count(*) FROM outbox")
            .expect("count"),
        0
    );
    assert_eq!(
        store
            .query_i64("SELECT rev FROM patient_case")
            .expect("rev"),
        0,
        "the failed edit must not bump the local revision either"
    );

    store
        .exec("DROP TRIGGER fail_outbox")
        .expect("drop trigger");
    store
        .apply_local(case_id, &[(f, Value::Text("kept".into()))], CaseRev(0))
        .expect("apply_local");
    assert_eq!(
        store
            .query_i64("SELECT count(*) FROM field_value")
            .expect("count"),
        1
    );
    assert_eq!(
        store
            .query_i64("SELECT count(*) FROM outbox")
            .expect("count"),
        1
    );
}

#[test]
fn local_edits_are_marked_pending_and_bump_the_local_rev() {
    let (store, def, case_id) = fixture();
    let f = ids(&def)[0];
    store
        .apply_local(case_id, &[(f, Value::Text("x".into()))], CaseRev(0))
        .expect("apply_local");

    assert_eq!(
        store
            .query_i64("SELECT pending FROM field_value")
            .expect("pending"),
        1
    );
    let row = store.case(case_id).expect("case");
    assert_eq!(row.rev, CaseRev(1));
    assert_eq!(row.synced_rev, CaseRev(0), "nothing has been acked yet");
}

// ------------------------------------------------------------------ sync path

#[test]
fn apply_server_values_never_clobbers_a_pending_row() {
    let (store, def, case_id) = fixture();
    let f = ids(&def);
    // f[0] is edited locally and unsynced; f[1] is clean.
    store
        .apply_local(case_id, &[(f[0], Value::Text("mine".into()))], CaseRev(0))
        .expect("apply_local");

    let rows = vec![
        ValueRow {
            field_id: f[0],
            value: Value::Text("theirs".into()),
            rev: CaseRev(9),
            updated_by: Some(ActorId::new("other@example.test")),
            updated_at: Some("2026-08-17T10:00:00.000Z".into()),
        },
        ValueRow {
            field_id: f[1],
            value: Value::Num(parse_decimal("3.14").expect("decimal")),
            rev: CaseRev(9),
            updated_by: None,
            updated_at: None,
        },
    ];
    store
        .apply_server_values(case_id, &rows, CaseRev(9))
        .expect("apply_server_values");

    let got = value_map(store.load_case_values(case_id).expect("load"));
    assert_eq!(
        got.get(&f[0]),
        Some(&Value::Text("mine".into())),
        "an unsynced local edit must survive an inbound server value"
    );
    assert_eq!(
        got.get(&f[1]),
        Some(&Value::Num(parse_decimal("3.14").expect("decimal")))
    );
    let clean_pending: i64 = {
        let conn = store.reader().expect("reader");
        conn.query_row(
            "SELECT pending FROM field_value WHERE case_id = ?1 AND field_id = ?2",
            params![case_id.to_string(), f[1].to_string()],
            |r| r.get(0),
        )
        .expect("pending")
    };
    assert_eq!(clean_pending, 0, "the clean row lands with pending = 0");
    assert_eq!(store.case(case_id).expect("case").synced_rev, CaseRev(9));
}

#[test]
fn confirm_clears_pending_and_drops_the_queue_row() {
    let (store, def, case_id) = fixture();
    let f = ids(&def)[0];
    store
        .apply_local(case_id, &[(f, Value::Text("x".into()))], CaseRev(0))
        .expect("apply_local");

    store.confirm(case_id, &[f], CaseRev(4)).expect("confirm");

    assert_eq!(
        store
            .query_i64("SELECT count(*) FROM outbox")
            .expect("count"),
        0
    );
    assert_eq!(
        store
            .query_i64("SELECT pending FROM field_value")
            .expect("pending"),
        0
    );
    assert_eq!(
        store.query_i64("SELECT rev FROM field_value").expect("rev"),
        4
    );
    let row = store.case(case_id).expect("case");
    assert_eq!(row.synced_rev, CaseRev(4));

    // A confirmed field is no longer protected from inbound server values.
    store
        .apply_server_values(
            case_id,
            &[ValueRow {
                field_id: f,
                value: Value::Text("theirs".into()),
                rev: CaseRev(5),
                updated_by: None,
                updated_at: None,
            }],
            CaseRev(5),
        )
        .expect("apply_server_values");
    assert_eq!(
        value_map(store.load_case_values(case_id).expect("load")).get(&f),
        Some(&Value::Text("theirs".into()))
    );
}

#[test]
fn next_outbox_batch_is_ordered_and_limited() {
    let (store, def, case_id) = fixture();
    let f = ids(&def);
    for id in &f[..4] {
        store
            .apply_local(case_id, &[(*id, Value::Text("x".into()))], CaseRev(0))
            .expect("apply_local");
    }
    assert_eq!(store.next_outbox_batch(2).expect("batch").len(), 2);
    let all = store.next_outbox_batch(64).expect("batch");
    assert_eq!(all.len(), 4);
    assert!(
        all.windows(2)
            .all(|w| w[0].next_attempt_at <= w[1].next_attempt_at),
        "the batch must be ordered by next_attempt_at"
    );
}

#[test]
fn bump_attempts_records_the_error_and_defers_the_retry() {
    let (store, def, case_id) = fixture();
    let f = ids(&def)[0];
    store
        .apply_local(case_id, &[(f, Value::Text("x".into()))], CaseRev(0))
        .expect("apply_local");

    store
        .bump_attempts(case_id, f, "503 from origin")
        .expect("bump");

    assert_eq!(
        store
            .query_i64("SELECT attempts FROM outbox")
            .expect("attempts"),
        1
    );
    assert!(
        store.next_outbox_batch(64).expect("batch").is_empty(),
        "a backed-off row must not be handed out again immediately"
    );

    // Backoff schedule: min(60s, 2^attempts * 500ms).
    assert_eq!(outbox::backoff(0), std::time::Duration::from_millis(500));
    assert_eq!(outbox::backoff(3), std::time::Duration::from_millis(4000));
    assert_eq!(outbox::backoff(20), std::time::Duration::from_secs(60));
}

#[test]
fn drop_outbox_removes_only_the_named_fields() {
    let (store, def, case_id) = fixture();
    let f = ids(&def);
    for id in &f[..3] {
        store
            .apply_local(case_id, &[(*id, Value::Text("x".into()))], CaseRev(0))
            .expect("apply_local");
    }
    store.drop_outbox(case_id, &[f[0], f[2]]).expect("drop");
    let left = store.next_outbox_batch(64).expect("batch");
    assert_eq!(left.len(), 1);
    assert_eq!(left[0].field_id, f[1]);
}

// ------------------------------------------------------------------ worklist

#[test]
fn worklist_is_most_recently_updated_first() {
    let store = Store::open_in_memory().expect("open");
    let def = seven_kind_form();
    store.save_form(&def, ConfigRev(1)).expect("save_form");

    let stamps = [
        "2026-08-15T08:00:00.000Z",
        "2026-08-17T08:00:00.000Z",
        "2026-08-16T08:00:00.000Z",
    ];
    let cases: Vec<CaseId> = stamps
        .iter()
        .map(|s| {
            let id = CaseId::new();
            store
                .upsert_case(&summary(id, def.form_id, s))
                .expect("upsert_case");
            id
        })
        .collect();

    // Somebody else's case must not appear.
    let other = CaseId::new();
    let mut theirs = summary(other, def.form_id, "2026-08-18T08:00:00.000Z");
    theirs.assignee = Some("someone.else@example.test".into());
    store.upsert_case(&theirs).expect("upsert_case");

    let list = store
        .worklist("abstractor@example.test", 10)
        .expect("worklist");
    assert_eq!(
        list.iter().map(|c| c.case_id).collect::<Vec<_>>(),
        vec![cases[1], cases[2], cases[0]]
    );
    assert_eq!(
        store
            .worklist("abstractor@example.test", 2)
            .expect("worklist")
            .len(),
        2
    );
}

#[test]
fn upsert_case_never_rewinds_a_revision() {
    let (store, def, case_id) = fixture();
    store
        .apply_server_values(case_id, &[], CaseRev(12))
        .expect("apply_server_values");
    store
        .upsert_case(&summary(case_id, def.form_id, "2026-08-17T12:00:00.000Z"))
        .expect("upsert_case");
    let row = store.case(case_id).expect("case");
    assert_eq!(row.synced_rev, CaseRev(12));
    assert_eq!(row.rev, CaseRev(12));
    assert_eq!(row.updated_at, "2026-08-17T12:00:00.000Z");
}

// ------------------------------------------------------------------ conflicts

fn conflicted() -> (Store, FormDef, CaseId, FieldId) {
    let (store, def, case_id) = fixture();
    let f = ids(&def)[0];
    store
        .apply_local(case_id, &[(f, Value::Text("mine".into()))], CaseRev(0))
        .expect("apply_local");
    let rows = vec![ValueRow {
        field_id: f,
        value: Value::Text("theirs".into()),
        rev: CaseRev(9),
        updated_by: Some(ActorId::new("other@example.test")),
        updated_at: Some("2026-08-17T10:00:00.000Z".into()),
    }];
    store
        .record_conflicts(case_id, &rows)
        .expect("record_conflicts");
    // The drain loop applies the server rev and drops the losing outbox row.
    store
        .apply_server_values(case_id, &rows, CaseRev(9))
        .expect("apply_server_values");
    store.drop_outbox(case_id, &[f]).expect("drop_outbox");
    (store, def, case_id, f)
}

#[test]
fn conflicts_are_recorded_with_both_sides() {
    let (store, _, case_id, f) = conflicted();
    let list = store.list_conflicts(case_id).expect("list");
    assert_eq!(list.len(), 1);
    assert_eq!(list[0].field_id, f);
    assert_eq!(list[0].mine, Value::Text("mine".into()));
    assert_eq!(list[0].theirs, Value::Text("theirs".into()));
    assert_eq!(list[0].theirs_by.as_deref(), Some("other@example.test"));
    assert_eq!(
        list[0].theirs_at.as_deref(),
        Some("2026-08-17T10:00:00.000Z")
    );
    assert!(
        store
            .list_conflicts(CaseId::new())
            .expect("list")
            .is_empty(),
        "conflicts are scoped to their case"
    );
}

#[test]
fn resolving_take_theirs_clears_the_local_value_and_the_conflict() {
    let (store, _, case_id, f) = conflicted();
    store
        .resolve_conflict(case_id, f, false)
        .expect("resolve_conflict");

    assert_eq!(
        value_map(store.load_case_values(case_id).expect("load")).get(&f),
        Some(&Value::Text("theirs".into()))
    );
    assert_eq!(
        store
            .query_i64("SELECT pending FROM field_value")
            .expect("pending"),
        0
    );
    assert!(store.list_conflicts(case_id).expect("list").is_empty());
    assert_eq!(
        store
            .query_i64("SELECT count(*) FROM outbox")
            .expect("count"),
        0
    );
}

#[test]
fn resolving_keep_mine_re_enqueues_against_the_new_base_rev() {
    let (store, _, case_id, f) = conflicted();
    store
        .resolve_conflict(case_id, f, true)
        .expect("resolve_conflict");

    assert_eq!(
        value_map(store.load_case_values(case_id).expect("load")).get(&f),
        Some(&Value::Text("mine".into()))
    );
    assert_eq!(
        store
            .query_i64("SELECT pending FROM field_value")
            .expect("pending"),
        1
    );
    assert!(store.list_conflicts(case_id).expect("list").is_empty());

    let batch = store.next_outbox_batch(64).expect("batch");
    assert_eq!(batch.len(), 1);
    assert_eq!(batch[0].value, Value::Text("mine".into()));
    assert_eq!(
        batch[0].base_rev,
        CaseRev(9),
        "the retry must be against the rev the server is now at"
    );
}

#[test]
fn resolving_an_unknown_conflict_is_an_error() {
    let (store, def, case_id) = fixture();
    assert!(matches!(
        store.resolve_conflict(case_id, ids(&def)[0], true),
        Err(StoreError::ConflictNotFound { .. })
    ));
}

// ------------------------------------------------------------------ sync state

#[test]
fn sync_state_round_trips_and_overwrites() {
    let (store, _, _) = fixture();
    assert_eq!(store.sync_state("cursor").expect("get"), None);
    store.set_sync_state("cursor", "abc").expect("set");
    assert_eq!(store.sync_state("cursor").expect("get"), Some("abc".into()));
    store.set_sync_state("cursor", "def").expect("set");
    assert_eq!(store.sync_state("cursor").expect("get"), Some("def".into()));
}

// ------------------------------------------------------------------ phi

/// Checklist item 5 in `docs/12-PHI-READINESS.md`. Uses an explicit key so the test never
/// touches the developer's real keychain.
#[cfg(feature = "phi")]
#[test]
fn wrong_key_fails() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("medatat.db");
    let right = "00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff";
    let wrong = "ffeeddccbbaa99887766554433221100ffeeddccbbaa99887766554433221100";

    let store = Store::open_with_key(&path, right).expect("open");
    store
        .save_form(&seven_kind_form(), ConfigRev(1))
        .expect("save_form");
    drop(store);

    match Store::open_with_key(&path, wrong) {
        Err(StoreError::Locked) => {}
        Err(e) => panic!("a wrong key must report Locked, got {e}"),
        Ok(_) => panic!("a wrong key must not open the database"),
    }

    // And the file is untouched: the right key still opens it.
    let store = Store::open_with_key(&path, right).expect("reopen");
    assert_eq!(store.load_all_forms().expect("forms").len(), 1);
}
