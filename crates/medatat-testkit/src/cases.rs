//! Synthetic case data: values that are valid for the field they belong to.
//!
//! Every generator is seeded, so a bench or a property failure reproduces exactly. The
//! values are shaped to pass `medatat_core::validate` — a fixture that generates garbage
//! would make every downstream test a validation test.

use crate::rng::Rng;
use crate::words::{GIVEN_NAMES, NOTE_WORDS, SURNAMES};
use chrono::{NaiveDate, NaiveTime};
use medatat_core::def::{FieldKind, FieldOption, FormDef};
use medatat_core::ids::{ActorId, CaseId, CaseRev, FieldId};
use medatat_core::value::Value;
use medatat_core::wire::ValueRow;
use rust_decimal::Decimal;
use rust_decimal::prelude::ToPrimitive;

/// Roughly this percentage of fields are left `Null`, because a real chart abstraction is
/// never complete. `Null` is valid for any kind, so this does not weaken the fixture.
const NULL_PERCENT: u64 = 8;

/// One case's worth of values: one entry per placed field, in render order.
pub fn synthetic_case(form: &FormDef, seed: u64) -> Vec<(FieldId, Value)> {
    let mut rng = Rng::new(seed);
    form.iter_fields()
        .map(|sf| (sf.field.field_id, value_for(&mut rng, &sf.field.kind)))
        .collect()
}

/// The same data as [`synthetic_case`], shaped as the rows a server or store hands back.
pub fn synthetic_value_rows(form: &FormDef, seed: u64, rev: CaseRev) -> Vec<ValueRow> {
    synthetic_case(form, seed)
        .into_iter()
        .map(|(field_id, value)| ValueRow {
            field_id,
            value,
            rev,
            updated_by: Some(synthetic_actor(seed)),
            updated_at: Some("2026-01-01T00:00:00Z".to_string()),
        })
        .collect()
}

/// A synthetic medical record number. Not a real MRN format for any real institution —
/// deliberately prefixed so it can never be mistaken for one.
pub fn synthetic_mrn(seed: u64) -> String {
    let mut rng = Rng::new(seed ^ 0x4D52_4E00);
    format!("SYN-MRN-{:08}", rng.below(100_000_000))
}

/// An invented patient name from the fixed word lists.
pub fn synthetic_name(seed: u64) -> String {
    let mut rng = Rng::new(seed ^ 0x4E41_4D45);
    format!("{} {}", rng.pick(GIVEN_NAMES), rng.pick(SURNAMES))
}

/// A stable case identity for a seed, so a corpus can be regenerated in place.
pub fn synthetic_case_id(seed: u64) -> CaseId {
    CaseId::from(Rng::new(seed ^ 0x4341_5345).uuid())
}

/// A synthetic actor. The real one is always resolved server-side from the session token.
pub fn synthetic_actor(seed: u64) -> ActorId {
    let mut rng = Rng::new(seed ^ 0x4143_544F);
    ActorId::new(format!("syn-user-{:04}", rng.below(50)))
}

/// A valid value for one field kind, or `Null`.
pub fn value_for(rng: &mut Rng, kind: &FieldKind) -> Value {
    if rng.chance(NULL_PERCENT) {
        return Value::Null;
    }
    match kind {
        FieldKind::Text { max_len } => text(rng, *max_len, 1, 4),
        FieldKind::Textarea { max_len, .. } => text(rng, *max_len, 6, 24),
        FieldKind::Numeric { min, max, scale } => numeric(rng, *min, *max, *scale),
        FieldKind::Date => date(rng),
        FieldKind::Time => time(rng),
        FieldKind::Radio { options } | FieldKind::Select { options, .. } => option(rng, options),
    }
}

fn text(rng: &mut Rng, max_len: Option<u32>, min_words: u64, max_words: u64) -> Value {
    let cap = max_len.map(|m| m as usize).unwrap_or(usize::MAX);
    if cap == 0 {
        return Value::Null;
    }
    let wanted = rng.range(min_words, max_words);
    let mut s = String::new();
    for _ in 0..wanted {
        let w = rng.pick(NOTE_WORDS);
        let extra = if s.is_empty() { w.len() } else { w.len() + 1 };
        if s.len() + extra > cap {
            break;
        }
        if !s.is_empty() {
            s.push(' ');
        }
        s.push_str(w);
    }
    if s.is_empty() {
        // Every word was too long for the cap. A single character is always in range, and
        // a non-empty string keeps `required` placements satisfiable.
        s.push('x');
    }
    Value::Text(s)
}

fn numeric(rng: &mut Rng, min: Option<Decimal>, max: Option<Decimal>, scale: u8) -> Value {
    // Generate the scaled integer mantissa, so the result's scale is exactly the field's
    // and `check_scale` can never trip.
    let scale = scale.min(9) as u32;
    let factor = Decimal::from(10u64.pow(scale));
    let lo = min.unwrap_or(Decimal::ZERO);
    let hi = max.unwrap_or(lo + Decimal::from(1000));

    let lo_m = (lo * factor).ceil().to_i64().unwrap_or(0);
    let hi_m = (hi * factor).floor().to_i64().unwrap_or(lo_m);
    Value::Num(Decimal::new(rng.range_i64(lo_m, hi_m), scale))
}

fn date(rng: &mut Rng) -> Value {
    let y = rng.range(2019, 2026) as i32;
    let m = rng.range(1, 12) as u32;
    // 1..=28 is a real day in every month of every year, leap or not.
    let d = rng.range(1, 28) as u32;
    match NaiveDate::from_ymd_opt(y, m, d) {
        Some(v) => Value::Date(v),
        None => Value::Null,
    }
}

fn time(rng: &mut Rng) -> Value {
    let h = rng.range(0, 23) as u32;
    let m = rng.range(0, 59) as u32;
    match NaiveTime::from_hms_opt(h, m, 0) {
        Some(v) => Value::Time(v),
        None => Value::Null,
    }
}

fn option(rng: &mut Rng, options: &[FieldOption]) -> Value {
    if options.is_empty() {
        return Value::Null;
    }
    Value::Opt(rng.pick(options).code.clone())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::forms::synthetic_form;
    use medatat_core::validate::validate;

    #[test]
    fn same_seed_produces_identical_values() {
        let form = synthetic_form(300);
        assert_eq!(synthetic_case(&form, 7), synthetic_case(&form, 7));
        assert_eq!(synthetic_mrn(7), synthetic_mrn(7));
        assert_eq!(synthetic_name(7), synthetic_name(7));
        assert_eq!(synthetic_case_id(7), synthetic_case_id(7));
    }

    #[test]
    fn different_seeds_produce_different_values() {
        let form = synthetic_form(300);
        assert_ne!(synthetic_case(&form, 1), synthetic_case(&form, 2));
        assert_ne!(synthetic_mrn(1), synthetic_mrn(2));
    }

    #[test]
    fn every_generated_value_validates() {
        // Several seeds, because a single one can miss a kind/constraint combination.
        for seed in 0..25u64 {
            let form = synthetic_form_for(seed);
            for (i, (field_id, value)) in synthetic_case(&form, seed).into_iter().enumerate() {
                let idx = form.idx_of(field_id).expect("generated id must be placed");
                let sf = form.field_at(idx).expect("index must resolve");
                assert_eq!(
                    validate(&sf.field, &value),
                    Ok(()),
                    "seed {seed} field {i} kind {} value {value:?}",
                    sf.field.kind.tag()
                );
            }
        }
    }

    fn synthetic_form_for(seed: u64) -> FormDef {
        crate::forms::synthetic_form_seeded(120, seed)
    }

    #[test]
    fn one_value_per_placed_field() {
        let form = synthetic_form(500);
        assert_eq!(synthetic_case(&form, 3).len(), 500);
    }

    #[test]
    fn cases_are_neither_empty_nor_full() {
        let form = synthetic_form(500);
        let values = synthetic_case(&form, 11);
        let nulls = values.iter().filter(|(_, v)| v.is_null()).count();
        assert!(nulls > 0, "a real abstraction is never complete");
        assert!(nulls < values.len() / 2, "too sparse to be useful: {nulls}");
    }

    #[test]
    fn mrn_is_obviously_synthetic() {
        for seed in 0..100 {
            assert!(synthetic_mrn(seed).starts_with("SYN-MRN-"));
        }
    }

    #[test]
    fn value_rows_carry_the_requested_rev() {
        let form = synthetic_form(20);
        let rows = synthetic_value_rows(&form, 4, CaseRev(9));
        assert_eq!(rows.len(), 20);
        assert!(rows.iter().all(|r| r.rev == CaseRev(9)));
    }
}
