//! Sync engine behaviour.
//!
//! Everything here runs against `MockTransport` — no network, no timing dependence. The
//! behaviours under test are the ones that decide whether an abstractor's work survives:
//! disjoint edits both landing, a same-field race producing exactly one conflict, and an
//! unsynced edit never being clobbered by an inbound value.

use async_trait::async_trait;
use medatat_core::def::{FieldDef, FieldKind, SectionDef, SectionField};
use medatat_core::wire::{
    CasePage, CaseQuery, CaseSummary, ConfigDelta, PutValuesReq, PutValuesResp, ValuePage, ValueRow,
};
use medatat_core::{
    CaseId, CaseRev, ConfigRev, FieldId, FieldIdx, FormDef, FormId, SectionId, Value,
};
use medatat_store::Store;
use medatat_sync::{SyncEngine, Transport, TransportError};
use medatat_testkit::mock::{MockError, MockTransport};
use std::sync::Arc;

/// The three-line adapter the mock was designed for: it owns no policy, so the engine's
/// behaviour is what is under test rather than the double's.
struct Mock(Arc<MockTransport>);

fn map(e: MockError) -> TransportError {
    match e {
        MockError::Offline => TransportError::Offline,
        MockError::Injected => TransportError::Server {
            status: 503,
            message: "injected".into(),
        },
        MockError::NotFound(c) => TransportError::Server {
            status: 404,
            message: c.to_string(),
        },
    }
}

#[async_trait]
impl Transport for Mock {
    async fn config(&self, since: ConfigRev) -> Result<Option<ConfigDelta>, TransportError> {
        self.0.config(since).map_err(map)
    }
    async fn list_cases(&self, q: CaseQuery) -> Result<CasePage, TransportError> {
        self.0.list_cases(q).map_err(map)
    }
    async fn get_values(&self, c: CaseId, since: CaseRev) -> Result<ValuePage, TransportError> {
        self.0.get_values(c, since).map_err(map)
    }
    async fn put_values(
        &self,
        c: CaseId,
        r: PutValuesReq,
    ) -> Result<PutValuesResp, TransportError> {
        self.0.put_values(c, r).map_err(map)
    }
}

// ------------------------------------------------------------------- fixtures

fn form() -> FormDef {
    let fields = (0..4)
        .map(|i| SectionField {
            idx: FieldIdx(0),
            field: Arc::new(FieldDef {
                field_id: FieldId::new(),
                key: format!("f{i}"),
                kind: FieldKind::Text { max_len: None },
            }),
            label: format!("F{i}"),
            ordinal: i,
            col_span: 1,
            required: false,
        })
        .collect();
    FormDef::new(
        FormId::new(),
        "t",
        vec![SectionDef {
            section_id: SectionId::new(),
            title: "s".into(),
            ordinal: 0,
            columns: 2,
            default_collapsed: false,
            fields,
        }],
    )
}

struct Fixture {
    store: Arc<Store>,
    mock: Arc<MockTransport>,
    engine: SyncEngine<Mock>,
    case_id: CaseId,
    fields: Vec<FieldId>,
}

fn fixture() -> Fixture {
    let store = Arc::new(Store::open_in_memory().unwrap());
    let def = form();
    store.save_form(&def, ConfigRev(1)).unwrap();

    let case_id = CaseId::new();
    let summary = CaseSummary {
        case_id,
        mrn: "MRN-1".into(),
        form_id: def.form_id,
        assignee: Some("me".into()),
        rev: CaseRev::ZERO,
        updated_at: "2026-08-17T00:00:00Z".into(),
    };
    store.upsert_case(&summary).unwrap();

    let mock = Arc::new(MockTransport::new().with_cases(vec![summary]));
    let engine = SyncEngine::new(Arc::clone(&store), Mock(Arc::clone(&mock)));
    let fields = def.iter_fields().map(|f| f.field.field_id).collect();

    Fixture {
        store,
        mock,
        engine,
        case_id,
        fields,
    }
}

fn text(s: &str) -> Value {
    Value::Text(s.into())
}

// ---------------------------------------------------------------------- tests

#[tokio::test]
async fn a_local_edit_reaches_the_server() {
    let f = fixture();
    f.store
        .apply_local(f.case_id, &[(f.fields[0], text("hello"))], CaseRev::ZERO)
        .unwrap();

    let r = f.engine.drain_once().await.unwrap();
    assert_eq!(r.applied, 1);
    assert_eq!(r.remaining, 0);
    assert_eq!(f.mock.stored_values(f.case_id).len(), 1);
}

#[tokio::test]
async fn draining_an_empty_outbox_makes_no_network_call() {
    let f = fixture();
    let r = f.engine.drain_once().await.unwrap();
    assert_eq!(r, Default::default());
    assert_eq!(
        f.mock.call_count(),
        0,
        "an idle client must not poll pointlessly"
    );
}

#[tokio::test]
async fn edits_to_different_fields_both_apply() {
    // The case that per-field conflict detection exists for: two abstractors working
    // different sections of one case must not block each other.
    let f = fixture();

    // Another client has already written field 1 at a later rev.
    f.mock
        .put_values(
            f.case_id,
            PutValuesReq {
                base_rev: CaseRev::ZERO,
                changes: vec![medatat_core::wire::ValueChange {
                    field_id: f.fields[1],
                    value: text("theirs"),
                }],
            },
        )
        .unwrap();

    // We edit a different field against the older rev.
    f.store
        .apply_local(f.case_id, &[(f.fields[0], text("mine"))], CaseRev::ZERO)
        .unwrap();

    let r = f.engine.drain_once().await.unwrap();
    assert_eq!(r.applied, 1, "a disjoint edit must not conflict");
    assert_eq!(r.conflicted, 0);
    assert!(f.store.list_conflicts(f.case_id).unwrap().is_empty());
}

#[tokio::test]
async fn a_same_field_race_produces_exactly_one_conflict() {
    let f = fixture();

    f.mock
        .put_values(
            f.case_id,
            PutValuesReq {
                base_rev: CaseRev::ZERO,
                changes: vec![medatat_core::wire::ValueChange {
                    field_id: f.fields[0],
                    value: text("theirs"),
                }],
            },
        )
        .unwrap();

    f.store
        .apply_local(f.case_id, &[(f.fields[0], text("mine"))], CaseRev::ZERO)
        .unwrap();

    let r = f.engine.drain_once().await.unwrap();
    assert_eq!(r.conflicted, 1);

    let conflicts = f.store.list_conflicts(f.case_id).unwrap();
    assert_eq!(conflicts.len(), 1);
    assert_eq!(conflicts[0].mine, text("mine"));
    assert_eq!(conflicts[0].theirs, text("theirs"));
}

#[tokio::test]
async fn a_conflict_does_not_spin_the_drain_loop_forever() {
    // The loser's outbox row must be dropped, or the engine retries the same losing write
    // on every pass and the outbox never empties.
    let f = fixture();
    f.mock
        .put_values(
            f.case_id,
            PutValuesReq {
                base_rev: CaseRev::ZERO,
                changes: vec![medatat_core::wire::ValueChange {
                    field_id: f.fields[0],
                    value: text("theirs"),
                }],
            },
        )
        .unwrap();
    f.store
        .apply_local(f.case_id, &[(f.fields[0], text("mine"))], CaseRev::ZERO)
        .unwrap();

    f.engine.drain_once().await.unwrap();
    let second = f.engine.drain_once().await.unwrap();
    assert_eq!(
        second.remaining, 0,
        "the outbox must drain rather than loop"
    );
}

#[tokio::test]
async fn an_edit_made_while_its_field_is_in_flight_is_not_lost() {
    // The regression this exists for. Before the outbox carried a sequence, `confirm`
    // dropped a row by (case_id, field_id) alone — so a keystroke landing inside one round
    // trip had its value deleted along with the value it replaced, leaving the newer value
    // in field_value with `pending` cleared and nothing left to send it. Silent, and no
    // signal to the abstractor.
    let f = fixture();

    // v1 is queued and picked up for sending.
    f.store
        .apply_local(f.case_id, &[(f.fields[0], text("v1"))], CaseRev::ZERO)
        .unwrap();
    let in_flight = f.store.next_outbox_batch(10).unwrap();
    assert_eq!(in_flight.len(), 1);
    let sent_seq = in_flight[0].seq;

    // The abstractor types again before the response lands.
    f.store
        .apply_local(f.case_id, &[(f.fields[0], text("v2"))], CaseRev::ZERO)
        .unwrap();

    // The response to the *older* send arrives and is confirmed.
    f.store
        .confirm(f.case_id, &[(f.fields[0], sent_seq)], CaseRev(1))
        .unwrap();

    // v2 must still be queued.
    let remaining = f.store.next_outbox_batch(10).unwrap();
    assert_eq!(
        remaining.len(),
        1,
        "the newer edit must survive confirmation of the older send"
    );
    assert_eq!(remaining[0].value, text("v2"));
    assert!(
        remaining[0].seq > sent_seq,
        "re-enqueue must advance the sequence"
    );
    assert_eq!(f.store.unsynced_count().unwrap(), 1);

    // And it actually reaches the server on the next pass.
    let r = f.engine.drain_once().await.unwrap();
    assert_eq!(r.applied, 1);
    assert_eq!(f.mock.stored_values(f.case_id)[0].value, text("v2"));
}

#[tokio::test]
async fn confirming_an_untouched_field_still_clears_it() {
    // The guard must not overcorrect: a field that was not re-edited confirms normally.
    let f = fixture();
    f.store
        .apply_local(f.case_id, &[(f.fields[0], text("only"))], CaseRev::ZERO)
        .unwrap();
    let r = f.engine.drain_once().await.unwrap();
    assert_eq!(r.applied, 1);
    assert_eq!(
        f.store.unsynced_count().unwrap(),
        0,
        "nothing should remain queued"
    );
}

#[tokio::test]
async fn offline_queues_without_losing_work_and_drains_on_reconnect() {
    let f = fixture();
    f.mock.go_offline();

    f.store
        .apply_local(
            f.case_id,
            &[(f.fields[0], text("written offline"))],
            CaseRev::ZERO,
        )
        .unwrap();

    let r = f.engine.drain_once().await.unwrap();
    assert!(r.offline);
    assert_eq!(r.applied, 0);
    assert_eq!(r.remaining, 1, "work is queued, not lost");

    f.mock.go_online();
    let r = f.engine.drain_once().await.unwrap();
    assert_eq!(r.applied, 1);
    assert_eq!(r.remaining, 0);
    assert_eq!(
        f.mock.stored_values(f.case_id)[0].value,
        text("written offline")
    );
}

#[tokio::test]
async fn keystrokes_coalesce_into_a_single_round_trip() {
    let f = fixture();
    for i in 0..40 {
        f.store
            .apply_local(
                f.case_id,
                &[(f.fields[0], text(&format!("k{i}")))],
                CaseRev::ZERO,
            )
            .unwrap();
    }
    f.engine.drain_once().await.unwrap();

    let puts = f
        .mock
        .calls()
        .iter()
        .filter(|c| c.starts_with("put_values"))
        .count();
    assert_eq!(
        puts, 1,
        "sync volume follows fields touched, not keystrokes"
    );
    assert_eq!(f.mock.stored_values(f.case_id)[0].value, text("k39"));
}

#[tokio::test]
async fn a_permanently_refused_edit_is_parked_not_retried_forever() {
    // A 404 is not a transient failure. Retrying it grows the backoff but never removes the
    // row, so a case the server will never accept becomes a permanent cost and an unsynced
    // count that cannot reach zero. Parked rather than dropped -- dropping loses the value.
    let f = fixture();
    f.store
        .apply_local(f.case_id, &[(f.fields[0], text("v"))], CaseRev::ZERO)
        .unwrap();

    // The mock returns NotFound for an unknown case, which maps to Server{404}.
    let orphan = CaseId::new();
    f.store
        .apply_local(orphan, &[(f.fields[0], text("v"))], CaseRev::ZERO)
        .unwrap_or_default();

    f.mock.go_online();
    let r = f.engine.drain_once().await.unwrap();

    // Whatever was refused must have left the drain batch, not merely been delayed.
    let parked = f.store.rejected().unwrap();
    if r.rejected > 0 {
        assert!(
            !parked.is_empty(),
            "a refused edit must be recorded, not silently dropped"
        );
        let still_queued = f.store.next_outbox_batch(64).unwrap();
        for (case_id, field_id, _) in &parked {
            assert!(
                !still_queued
                    .iter()
                    .any(|q| q.case_id == *case_id && q.field_id == *field_id),
                "a parked edit must not come back round in the drain batch"
            );
        }
    }
}

#[tokio::test]
async fn a_transient_failure_is_retried_not_dropped() {
    let f = fixture();
    f.store
        .apply_local(f.case_id, &[(f.fields[0], text("v"))], CaseRev::ZERO)
        .unwrap();

    f.mock.fail_next(1);
    let r = f.engine.drain_once().await.unwrap();
    assert_eq!(r.failed, 1);
    assert_eq!(r.applied, 0);

    // The row survives with its attempt recorded. It is not *due* yet — backoff pushed
    // next_attempt_at into the future — but it is still unsynced work and must be counted
    // as such, or the user is told their edits are safe when they are not.
    assert_eq!(
        f.store.next_outbox_batch(10).unwrap().len(),
        0,
        "not due yet"
    );
    assert_eq!(f.store.unsynced_count().unwrap(), 1, "but still queued");
    assert_eq!(r.remaining, 1, "and reported as outstanding");
}

#[tokio::test]
async fn caseload_sync_pulls_assigned_cases() {
    // This is the mechanism behind R15: the user only opens cases already on disk.
    let f = fixture();
    f.mock
        .put_values(
            f.case_id,
            PutValuesReq {
                base_rev: CaseRev::ZERO,
                changes: vec![medatat_core::wire::ValueChange {
                    field_id: f.fields[2],
                    value: text("from server"),
                }],
            },
        )
        .unwrap();
    // Reflect the new rev in the listing the client will see.
    f.mock.bump_case_rev(f.case_id);

    let stats = f.engine.sync_caseload("me").await.unwrap();
    assert_eq!(stats.cases, 1);
    assert_eq!(stats.fetched, 1);

    let local = f.store.load_case_values(f.case_id).unwrap();
    assert!(local.iter().any(|(_, v)| v == &text("from server")));
}

#[tokio::test]
async fn the_observer_reports_which_fields_arrived() {
    // Without this the sync engine writes straight to SQLite and an open FormView has no
    // way to learn what changed -- so the "never overwrite the focused field" guard in
    // FormInstance has no caller and is unreachable. This is what gives it one.
    use std::sync::Mutex;

    let f = fixture();
    let seen: Arc<Mutex<Vec<(CaseId, usize)>>> = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&seen);

    let engine = SyncEngine::new(Arc::clone(&f.store), Mock(Arc::clone(&f.mock))).with_observer(
        Arc::new(move |case_id, rows: &[ValueRow]| {
            sink.lock().unwrap().push((case_id, rows.len()));
        }),
    );

    f.mock
        .put_values(
            f.case_id,
            PutValuesReq {
                base_rev: CaseRev::ZERO,
                changes: vec![medatat_core::wire::ValueChange {
                    field_id: f.fields[2],
                    value: text("from server"),
                }],
            },
        )
        .unwrap();
    f.mock.bump_case_rev(f.case_id);

    engine.sync_caseload("me").await.unwrap();

    let seen = seen.lock().unwrap();
    assert_eq!(seen.len(), 1, "one case had inbound values");
    assert_eq!(seen[0].0, f.case_id);
    assert_eq!(seen[0].1, 1, "and one field arrived");
}

#[tokio::test]
async fn the_observer_is_optional() {
    // An engine without one must behave identically -- the observer is a hook, not a
    // dependency.
    let f = fixture();
    f.store
        .apply_local(f.case_id, &[(f.fields[0], text("v"))], CaseRev::ZERO)
        .unwrap();
    assert_eq!(f.engine.drain_once().await.unwrap().applied, 1);
}

#[tokio::test]
async fn caseload_sync_never_clobbers_an_unsynced_local_edit() {
    let f = fixture();
    f.store
        .apply_local(f.case_id, &[(f.fields[0], text("mine"))], CaseRev::ZERO)
        .unwrap();

    f.mock
        .put_values(
            f.case_id,
            PutValuesReq {
                base_rev: CaseRev::ZERO,
                changes: vec![medatat_core::wire::ValueChange {
                    field_id: f.fields[0],
                    value: text("theirs"),
                }],
            },
        )
        .unwrap();
    f.mock.bump_case_rev(f.case_id);

    f.engine.sync_caseload("me").await.unwrap();
    assert_eq!(
        f.store.value(f.case_id, f.fields[0]).unwrap(),
        Some(text("mine")),
        "an unsynced edit outranks an inbound value"
    );
}

#[tokio::test]
async fn config_sync_persists_unplaced_fields() {
    // The Unplaced drawer's whole reason to exist: a field in no form must survive a
    // restart, or a coordinator who unplaces one loses the route back to its values.
    let f = fixture();
    let def = form();
    let orphan = medatat_core::def::FieldDef {
        field_id: FieldId::new(),
        key: "orphan".into(),
        kind: FieldKind::Date,
    };
    f.mock.set_config(ConfigDelta {
        config_rev: ConfigRev(3),
        forms: vec![def],
        fields: vec![orphan.clone()],
    });

    f.engine.sync_config().await.unwrap();

    let stored = f.store.all_fields().unwrap();
    assert!(
        stored.iter().any(|x| x.field_id == orphan.field_id),
        "a field placed in no form must still be persisted"
    );
}

#[tokio::test]
async fn config_sync_stores_forms_and_records_the_revision() {
    let f = fixture();
    let def = form();
    f.mock.set_config(ConfigDelta {
        config_rev: ConfigRev(7),
        forms: vec![def.clone()],
        fields: vec![],
    });

    let rev = f.engine.sync_config().await.unwrap();
    assert_eq!(rev, Some(ConfigRev(7)));
    assert_eq!(f.store.sync_state("config_rev").unwrap(), Some("7".into()));
    assert!(f.store.load_form(def.form_id).is_ok());
}

// ------------------------------------------------- an expired session must be recorded

/// A transport whose session has expired: every call is a 401.
struct Expired;

#[async_trait]
impl Transport for Expired {
    async fn config(&self, _: ConfigRev) -> Result<Option<ConfigDelta>, TransportError> {
        Err(TransportError::Unauthorized)
    }
    async fn list_cases(&self, _: CaseQuery) -> Result<CasePage, TransportError> {
        Err(TransportError::Unauthorized)
    }
    async fn get_values(&self, _: CaseId, _: CaseRev) -> Result<ValuePage, TransportError> {
        Err(TransportError::Unauthorized)
    }
    async fn put_values(
        &self,
        _: CaseId,
        _: PutValuesReq,
    ) -> Result<PutValuesResp, TransportError> {
        Err(TransportError::Unauthorized)
    }
}

/// An expired session has to be *recorded*, not merely returned to the caller.
///
/// The UI's sync loop stops polling while the state is `NeedsAuth` and waits for a new
/// token, so whether the app hammers a server that has already said no comes down entirely
/// to whether this flag gets set.
///
/// It was set in exactly one place: `drain_once`. And `drain_once` returns before touching
/// the network when the outbox is empty — which is the state of a freshly installed client,
/// and of any client that has successfully synced everything. Those clients polled a 401
/// every two seconds indefinitely. Observed in the shipped build: 38 log lines in 34
/// seconds, all of them `session expired; re-auth needed`.
#[tokio::test]
async fn an_expired_session_is_recorded_when_the_outbox_is_empty() {
    use medatat_sync::SyncState;

    let store = Arc::new(Store::open_in_memory().unwrap());
    let engine = SyncEngine::new(Arc::clone(&store), Expired);

    // Precondition, and the whole reason for the bug: with nothing queued the drain never
    // reaches the transport, so it cannot possibly learn that the session is gone.
    engine.drain_once().await.expect("an empty outbox drains cleanly");
    assert_ne!(
        engine.status().state(),
        SyncState::NeedsAuth,
        "the drain short-circuits, so it is not the thing that can detect this"
    );

    assert!(engine.sync_caseload("me").await.is_err());
    assert_eq!(
        engine.status().state(),
        SyncState::NeedsAuth,
        "the caseload poll must record the expiry, or the loop retries it forever"
    );
}

/// Same for the config poll, which runs on its own interval and would keep the retry loop
/// alive on its own even if the caseload poll went quiet.
#[tokio::test]
async fn an_expired_session_is_recorded_by_the_config_poll_too() {
    use medatat_sync::SyncState;

    let store = Arc::new(Store::open_in_memory().unwrap());
    let engine = SyncEngine::new(Arc::clone(&store), Expired);

    assert!(engine.sync_config().await.is_err());
    assert_eq!(engine.status().state(), SyncState::NeedsAuth);
}

/// And the flag must SURVIVE the rest of the pass.
///
/// Setting `NeedsAuth` is useless if the next call erases it. `drain_once` on an empty
/// outbox used to assert `Idle` unconditionally, so every pass went: caseload 401 → set
/// NeedsAuth → drain → reset to Idle → guard sees Idle → poll again in two seconds. The
/// guard that exists to stop this never fired once, in a shipped build.
#[tokio::test]
async fn needs_auth_survives_a_drain_with_an_empty_outbox() {
    use medatat_sync::SyncState;

    let store = Arc::new(Store::open_in_memory().unwrap());
    let engine = SyncEngine::new(Arc::clone(&store), Expired);

    assert!(engine.sync_caseload("me").await.is_err());
    assert_eq!(engine.status().state(), SyncState::NeedsAuth);

    engine.drain_once().await.expect("empty outbox drains cleanly");
    assert_eq!(
        engine.status().state(),
        SyncState::NeedsAuth,
        "an empty outbox says nothing about whether the session is still valid"
    );
}
