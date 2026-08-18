//! The write path (R2, R16). Pure over [`CaseStore`], so every rule below is exercised
//! natively by the tests at the bottom of this file — no workerd, no WASM.

use crate::error::{LogicError, LogicResult};
use crate::logic::case_store::{CaseStore, FieldLookup};
use medatat_core::ids::{ActorId, CaseId, CaseRev, FieldId};
use medatat_core::validate::validate;
use medatat_core::wire::{PutValuesReq, PutValuesResp, ValuePage, ValueRow};
use std::collections::HashSet;

/// Read every value with `rev > since_rev`. `None` reads the whole case.
pub fn handle_get_values<S: CaseStore>(
    store: &S,
    case_id: CaseId,
    since_rev: Option<CaseRev>,
) -> LogicResult<ValuePage> {
    let since = since_rev.unwrap_or(CaseRev::ZERO);
    Ok(ValuePage {
        case_id,
        rev: store.rev()?,
        values: store.get_all(since)?,
    })
}

/// Apply a batch of field changes, or reject the whole batch.
///
/// The order is load-bearing:
///
/// 1. **Actor first.** `actor` is resolved from the bearer token by the caller and is never
///    read from the request body. An unresolved actor refuses the write outright.
/// 2. **Validate every value** with `medatat_core::validate` — the same function the client
///    runs on each keystroke, so client feel and server truth cannot drift.
/// 3. **Conflict detection per field**, not per case: only rows among the *changed* fields
///    with `rev > base_rev` conflict. Two abstractors editing different fields of one case
///    both succeed. Any conflict rejects the entire batch and returns the server's rows.
/// 4. Apply, bumping the case rev exactly once.
pub fn handle_put_values<S: CaseStore>(
    store: &S,
    req: PutValuesReq,
    actor: &ActorId,
    defs: &FieldLookup,
) -> LogicResult<PutValuesResp> {
    if actor.as_str().trim().is_empty() {
        return Err(LogicError::NoActor);
    }

    let mut seen: HashSet<FieldId> = HashSet::with_capacity(req.changes.len());
    for change in &req.changes {
        if !seen.insert(change.field_id) {
            return Err(LogicError::Validation(format!(
                "field {} appears twice in one batch",
                change.field_id
            )));
        }
    }

    // An empty batch is a no-op, not a revision bump. The outbox coalesces, so a client
    // that raced itself to empty must not be able to inflate the case rev.
    if req.changes.is_empty() {
        return Ok(PutValuesResp::Applied {
            rev: store.rev()?,
            applied: Vec::new(),
        });
    }

    for change in &req.changes {
        let def = defs
            .get(change.field_id)
            .ok_or(LogicError::UnknownField(change.field_id))?;
        validate(def, &change.value).map_err(|e| LogicError::invalid(change.field_id, e))?;
    }

    let changed: Vec<FieldId> = req.changes.iter().map(|c| c.field_id).collect();
    let conflicts = store.changed_since(&changed, req.base_rev)?;
    if !conflicts.is_empty() {
        return Ok(PutValuesResp::Conflict {
            server_rev: store.rev()?,
            conflicts,
        });
    }

    let new_rev = store.rev()?.next();
    store.put(&req.changes, new_rev, actor)?;
    Ok(PutValuesResp::Applied {
        rev: new_rev,
        applied: changed,
    })
}

/// Convenience for the binding layer: the rows a conflict response should carry.
pub fn conflict_rows(resp: &PutValuesResp) -> &[ValueRow] {
    match resp {
        PutValuesResp::Conflict { conflicts, .. } => conflicts,
        PutValuesResp::Applied { .. } => &[],
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::logic::case_store::MemCaseStore;
    use medatat_core::def::{FieldDef, FieldKind, FieldOption};
    use medatat_core::error::ValidationError;
    use medatat_core::ids::OptionCode;
    use medatat_core::value::{Value, parse_decimal, parse_time_24};
    use medatat_core::wire::ValueChange;
    use std::sync::Arc;

    fn def(kind: FieldKind) -> FieldDef {
        FieldDef {
            field_id: FieldId::new(),
            key: format!("k{}", FieldId::new()),
            kind,
        }
    }

    fn text_def() -> FieldDef {
        def(FieldKind::Text { max_len: Some(64) })
    }

    fn time_def() -> FieldDef {
        def(FieldKind::Time)
    }

    fn numeric_def() -> FieldDef {
        def(FieldKind::Numeric {
            min: Some(parse_decimal("0").unwrap()),
            max: Some(parse_decimal("300").unwrap()),
            scale: 2,
        })
    }

    fn actor() -> ActorId {
        ActorId::new("user-1")
    }

    fn change(f: &FieldDef, v: Value) -> ValueChange {
        ValueChange {
            field_id: f.field_id,
            value: v,
        }
    }

    fn put(
        store: &MemCaseStore,
        defs: &FieldLookup,
        base_rev: i64,
        changes: Vec<ValueChange>,
    ) -> LogicResult<PutValuesResp> {
        handle_put_values(
            store,
            PutValuesReq {
                base_rev: CaseRev(base_rev),
                changes,
            },
            &actor(),
            defs,
        )
    }

    fn applied_rev(resp: &PutValuesResp) -> i64 {
        match resp {
            PutValuesResp::Applied { rev, .. } => rev.0,
            PutValuesResp::Conflict { server_rev, .. } => {
                panic!("expected Applied, got Conflict at {server_rev}")
            }
        }
    }

    // ------------------------------------------------------------------ rev

    #[test]
    fn r16_rev_is_monotonic_and_bumps_once_per_batch() {
        let a = text_def();
        let b = text_def();
        let defs = FieldLookup::from_defs([a.clone(), b.clone()]);
        let store = MemCaseStore::new();

        let r1 = put(
            &store,
            &defs,
            0,
            vec![
                change(&a, Value::Text("one".into())),
                change(&b, Value::Text("two".into())),
            ],
        )
        .unwrap();
        assert_eq!(applied_rev(&r1), 1, "two fields in one batch is one rev");

        let r2 = put(
            &store,
            &defs,
            1,
            vec![change(&a, Value::Text("three".into()))],
        )
        .unwrap();
        assert_eq!(applied_rev(&r2), 2);
        assert_eq!(store.rev().unwrap(), CaseRev(2));
    }

    #[test]
    fn r16_empty_batch_does_not_bump_rev() {
        let store = MemCaseStore::new();
        let defs = FieldLookup::new();
        let resp = put(&store, &defs, 0, vec![]).unwrap();
        assert_eq!(applied_rev(&resp), 0);
        assert_eq!(store.rev().unwrap(), CaseRev::ZERO);
    }

    // ------------------------------------------------- per-field conflicts

    #[test]
    fn r16_disjoint_field_edits_by_two_clients_both_apply() {
        let a = text_def();
        let b = text_def();
        let defs = FieldLookup::from_defs([a.clone(), b.clone()]);
        let store = MemCaseStore::new();

        // Both clients read the case at rev 0 and edit different fields.
        let first = put(
            &store,
            &defs,
            0,
            vec![change(&a, Value::Text("alice".into()))],
        )
        .unwrap();
        assert_eq!(applied_rev(&first), 1);

        let second = put(
            &store,
            &defs,
            0,
            vec![change(&b, Value::Text("bob".into()))],
        )
        .unwrap();
        assert_eq!(
            applied_rev(&second),
            2,
            "a stale base_rev must not conflict when the fields are disjoint"
        );

        assert_eq!(
            store.value_of(a.field_id),
            Some(Value::Text("alice".into()))
        );
        assert_eq!(store.value_of(b.field_id), Some(Value::Text("bob".into())));
    }

    #[test]
    fn r16_same_field_race_conflicts_and_returns_the_server_row() {
        let a = text_def();
        let defs = FieldLookup::from_defs([a.clone()]);
        let store = MemCaseStore::new();
        store.seed(
            a.field_id,
            Value::Text("server wins".into()),
            CaseRev(7),
            "user-2",
        );

        let resp = put(
            &store,
            &defs,
            3,
            vec![change(&a, Value::Text("client loses".into()))],
        )
        .unwrap();

        match resp {
            PutValuesResp::Conflict {
                server_rev,
                conflicts,
            } => {
                assert_eq!(server_rev, CaseRev(7));
                assert_eq!(conflicts.len(), 1);
                assert_eq!(conflicts[0].field_id, a.field_id);
                assert_eq!(conflicts[0].value, Value::Text("server wins".into()));
                assert_eq!(conflicts[0].rev, CaseRev(7));
                assert_eq!(conflicts[0].updated_by, Some(ActorId::new("user-2")));
                assert!(conflicts[0].updated_at.is_some());
            }
            other => panic!("expected Conflict, got {other:?}"),
        }

        assert_eq!(
            store.value_of(a.field_id),
            Some(Value::Text("server wins".into())),
            "a conflicted batch must not have written anything"
        );
    }

    #[test]
    fn r16_one_conflicted_field_rejects_the_whole_batch() {
        let a = text_def();
        let b = text_def();
        let c = text_def();
        let defs = FieldLookup::from_defs([a.clone(), b.clone(), c.clone()]);
        let store = MemCaseStore::new();
        store.seed(
            b.field_id,
            Value::Text("theirs".into()),
            CaseRev(5),
            "user-2",
        );

        let resp = put(
            &store,
            &defs,
            2,
            vec![
                change(&a, Value::Text("mine-a".into())),
                change(&b, Value::Text("mine-b".into())),
                change(&c, Value::Text("mine-c".into())),
            ],
        )
        .unwrap();

        assert!(matches!(resp, PutValuesResp::Conflict { .. }));
        assert_eq!(
            conflict_rows(&resp).len(),
            1,
            "only the racing field is reported"
        );
        assert_eq!(conflict_rows(&resp)[0].field_id, b.field_id);
        assert_eq!(store.value_of(a.field_id), None, "batch is all-or-nothing");
        assert_eq!(store.value_of(c.field_id), None, "batch is all-or-nothing");
    }

    #[test]
    fn r16_a_field_changed_at_or_below_base_rev_is_not_a_conflict() {
        let a = text_def();
        let defs = FieldLookup::from_defs([a.clone()]);
        let store = MemCaseStore::new();
        store.seed(a.field_id, Value::Text("old".into()), CaseRev(4), "user-2");

        // base_rev == the row's rev: the client has already seen this value.
        let resp = put(
            &store,
            &defs,
            4,
            vec![change(&a, Value::Text("new".into()))],
        )
        .unwrap();
        assert_eq!(applied_rev(&resp), 5);
        assert_eq!(store.value_of(a.field_id), Some(Value::Text("new".into())));
    }

    #[test]
    fn r16_conflict_detection_ignores_untouched_fields() {
        let a = text_def();
        let b = text_def();
        let defs = FieldLookup::from_defs([a.clone(), b.clone()]);
        let store = MemCaseStore::new();
        // b moved far ahead, but this batch does not touch b.
        store.seed(
            b.field_id,
            Value::Text("busy".into()),
            CaseRev(99),
            "user-2",
        );

        let resp = put(
            &store,
            &defs,
            0,
            vec![change(&a, Value::Text("quiet".into()))],
        )
        .unwrap();
        assert_eq!(applied_rev(&resp), 100);
    }

    // ------------------------------------------------- server-side validation

    #[test]
    fn r8_invalid_time_is_rejected_with_the_offending_field_id() {
        let t = time_def();
        let defs = FieldLookup::from_defs([t.clone()]);
        let store = MemCaseStore::new();

        // 24:00 cannot be constructed as a NaiveTime at all — that is the R8 gate.
        assert!(parse_time_24("24:00").is_err());

        // What a hostile client *can* send is a well-formed value of the wrong shape.
        let err = put(
            &store,
            &defs,
            0,
            vec![change(&t, Value::Text("24:00".into()))],
        )
        .unwrap_err();
        match err {
            LogicError::Invalid(ref fe) => {
                assert_eq!(fe.field_id, t.field_id);
                assert_eq!(fe.error, ValidationError::WrongType);
            }
            other => panic!("expected Invalid, got {other:?}"),
        }
        assert_eq!(err.http_status(), 422);
        assert_eq!(store.rev().unwrap(), CaseRev::ZERO, "nothing was written");
    }

    #[test]
    fn r8_a_time_outside_24_hours_cannot_even_be_deserialised() {
        // A Time crosses the wire as `HH:MM`, parsed by `parse_time_24`, so "24:00" is
        // rejected before any handler sees it. That is the first of the two R8 gates; the
        // second is the WrongType check below, for a client that sends the value as text.
        let id = FieldId::new();
        for bad in ["24:00", "23:60", "-1:00", "24:00:00", "9:5x"] {
            let json = format!(r#"{{"field_id":"{id}","value":{{"Time":"{bad}"}}}}"#);
            assert!(
                serde_json::from_str::<ValueChange>(&json).is_err(),
                "{bad} must not deserialise into a Time value"
            );
        }

        let parsed: ValueChange = serde_json::from_str(&format!(
            r#"{{"field_id":"{id}","value":{{"Time":"23:59"}}}}"#
        ))
        .expect("the canonical HH:MM form must parse");
        assert_eq!(parsed.value, Value::Time(parse_time_24("23:59").unwrap()));

        // `HH:MM:SS` is still accepted on read so a payload from an older build survives.
        let legacy: ValueChange = serde_json::from_str(&format!(
            r#"{{"field_id":"{id}","value":{{"Time":"23:59:00"}}}}"#
        ))
        .expect("the legacy HH:MM:SS form must still parse");
        assert_eq!(legacy.value, parsed.value);

        // What goes back out is always the documented `HH:MM`.
        let out = serde_json::to_value(&parsed.value).unwrap();
        assert_eq!(out, serde_json::json!({ "Time": "23:59" }));
    }

    #[test]
    fn r6_out_of_range_numeric_is_rejected_with_the_offending_field_id() {
        let n = numeric_def();
        let defs = FieldLookup::from_defs([n.clone()]);
        let store = MemCaseStore::new();

        let err = put(
            &store,
            &defs,
            0,
            vec![change(&n, Value::Num(parse_decimal("301").unwrap()))],
        )
        .unwrap_err();
        match err {
            LogicError::Invalid(ref fe) => {
                assert_eq!(fe.field_id, n.field_id);
                assert!(matches!(fe.error, ValidationError::AboveMax { .. }));
            }
            other => panic!("expected Invalid, got {other:?}"),
        }
        assert_eq!(err.http_status(), 422);
    }

    #[test]
    fn r6_excess_decimal_scale_is_rejected() {
        let n = numeric_def();
        let defs = FieldLookup::from_defs([n.clone()]);
        let store = MemCaseStore::new();
        let err = put(
            &store,
            &defs,
            0,
            vec![change(&n, Value::Num(parse_decimal("1.555").unwrap()))],
        )
        .unwrap_err();
        assert!(matches!(
            err,
            LogicError::Invalid(ref fe) if matches!(fe.error, ValidationError::TooManyDecimals { max: 2 })
        ));
    }

    #[test]
    fn r9_unknown_option_code_is_rejected() {
        let r = def(FieldKind::Radio {
            options: Arc::from(vec![FieldOption {
                code: OptionCode::new("M"),
                label: "Male".into(),
                ordinal: 0,
            }]),
        });
        let defs = FieldLookup::from_defs([r.clone()]);
        let store = MemCaseStore::new();
        let err = put(
            &store,
            &defs,
            0,
            vec![change(&r, Value::Opt(OptionCode::new("X")))],
        )
        .unwrap_err();
        assert!(matches!(
            err,
            LogicError::Invalid(ref fe) if fe.field_id == r.field_id
        ));
    }

    #[test]
    fn one_invalid_value_rejects_the_whole_batch() {
        let good = text_def();
        let bad = numeric_def();
        let defs = FieldLookup::from_defs([good.clone(), bad.clone()]);
        let store = MemCaseStore::new();

        let err = put(
            &store,
            &defs,
            0,
            vec![
                change(&good, Value::Text("fine".into())),
                change(&bad, Value::Num(parse_decimal("999").unwrap())),
            ],
        )
        .unwrap_err();
        assert!(matches!(err, LogicError::Invalid(_)));
        assert_eq!(store.value_of(good.field_id), None);
    }

    #[test]
    fn an_unconfigured_field_id_is_not_found() {
        let ghost = text_def();
        let defs = FieldLookup::new();
        let store = MemCaseStore::new();
        let err = put(
            &store,
            &defs,
            0,
            vec![change(&ghost, Value::Text("x".into()))],
        )
        .unwrap_err();
        assert_eq!(err, LogicError::UnknownField(ghost.field_id));
        assert_eq!(err.http_status(), 404);
    }

    #[test]
    fn a_duplicated_field_in_one_batch_is_rejected() {
        let a = text_def();
        let defs = FieldLookup::from_defs([a.clone()]);
        let store = MemCaseStore::new();
        let err = put(
            &store,
            &defs,
            0,
            vec![
                change(&a, Value::Text("first".into())),
                change(&a, Value::Text("second".into())),
            ],
        )
        .unwrap_err();
        assert_eq!(err.http_status(), 422);
    }

    // ------------------------------------------------------------- the actor

    #[test]
    fn a_write_with_no_resolved_actor_is_refused() {
        let a = text_def();
        let defs = FieldLookup::from_defs([a.clone()]);
        let store = MemCaseStore::new();

        for empty in ["", "   "] {
            let err = handle_put_values(
                &store,
                PutValuesReq {
                    base_rev: CaseRev::ZERO,
                    changes: vec![change(&a, Value::Text("x".into()))],
                },
                &ActorId::new(empty),
                &defs,
            )
            .unwrap_err();
            assert_eq!(err, LogicError::NoActor);
            assert_eq!(err.http_status(), 401);
        }
        assert_eq!(store.rev().unwrap(), CaseRev::ZERO);
    }

    #[test]
    fn the_actor_is_stamped_on_every_written_row() {
        let a = text_def();
        let b = text_def();
        let defs = FieldLookup::from_defs([a.clone(), b.clone()]);
        let store = MemCaseStore::new();
        handle_put_values(
            &store,
            PutValuesReq {
                base_rev: CaseRev::ZERO,
                changes: vec![
                    change(&a, Value::Text("x".into())),
                    change(&b, Value::Text("y".into())),
                ],
            },
            &ActorId::new("abstractor-77"),
            &defs,
        )
        .unwrap();
        assert_eq!(
            store.actor_of(a.field_id),
            Some(ActorId::new("abstractor-77"))
        );
        assert_eq!(
            store.actor_of(b.field_id),
            Some(ActorId::new("abstractor-77"))
        );
    }

    // ------------------------------------------------------------------ reads

    #[test]
    fn get_values_honours_since_rev() {
        let a = text_def();
        let b = text_def();
        let defs = FieldLookup::from_defs([a.clone(), b.clone()]);
        let store = MemCaseStore::new();
        put(
            &store,
            &defs,
            0,
            vec![change(&a, Value::Text("one".into()))],
        )
        .unwrap();
        put(
            &store,
            &defs,
            1,
            vec![change(&b, Value::Text("two".into()))],
        )
        .unwrap();

        let full = handle_get_values(&store, CaseId::new(), None).unwrap();
        assert_eq!(full.rev, CaseRev(2));
        assert_eq!(full.values.len(), 2);

        let delta = handle_get_values(&store, CaseId::new(), Some(CaseRev(1))).unwrap();
        assert_eq!(delta.values.len(), 1);
        assert_eq!(delta.values[0].field_id, b.field_id);
    }

    #[test]
    fn storage_failure_propagates_and_is_a_500() {
        let a = text_def();
        let defs = FieldLookup::from_defs([a.clone()]);
        let store = MemCaseStore::new();
        store.fail_with("sql exploded");
        let err = put(&store, &defs, 0, vec![change(&a, Value::Text("x".into()))]).unwrap_err();
        assert_eq!(err.http_status(), 500);
    }

    #[test]
    fn a_null_value_clears_a_field_and_is_always_valid() {
        let a = numeric_def();
        let defs = FieldLookup::from_defs([a.clone()]);
        let store = MemCaseStore::new();
        put(
            &store,
            &defs,
            0,
            vec![change(&a, Value::Num(parse_decimal("12.50").unwrap()))],
        )
        .unwrap();
        put(&store, &defs, 1, vec![change(&a, Value::Null)]).unwrap();
        assert_eq!(store.value_of(a.field_id), Some(Value::Null));
    }
}
