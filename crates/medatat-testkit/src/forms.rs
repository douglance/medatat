//! Synthetic `FormDef` generation.
//!
//! A generated form is deliberately *awkward*: all seven kinds (R5–R11), sections at one,
//! two, and three columns (R12), and multi-column spans. A benchmark run against a form of
//! 500 identical text fields would prove nothing about the real thing.

use crate::rng::Rng;
use crate::words::{FIELD_LABELS, RADIO_SETS, SECTION_TITLES, SELECT_SETS, UNITS};
use medatat_core::def::{FieldDef, FieldKind, FieldOption, FormDef, SectionDef, SectionField};
use medatat_core::ids::{FieldId, FieldIdx, FormId, OptionCode, SectionId};
use rust_decimal::Decimal;
use std::sync::Arc;

/// The seed used by [`synthetic_form`]. Frozen: changing it changes every default fixture
/// and every bench baseline at once.
pub const DEFAULT_SEED: u64 = 0x6D65_6461_7461_7401;

/// R12 column counts, cycled so any form with three or more sections exercises all three.
const COLUMN_CYCLE: [u8; 3] = [1, 2, 3];

/// The number of `FieldKind` variants. Fields cycle through them, so a form of seven or
/// more fields contains every kind.
const KIND_COUNT: usize = 7;

/// A form of `field_count` fields at the default seed.
pub fn synthetic_form(field_count: usize) -> FormDef {
    synthetic_form_seeded(field_count, DEFAULT_SEED)
}

/// A form of `field_count` fields. The same `seed` always produces a byte-identical form,
/// including every generated `FieldId`.
pub fn synthetic_form_seeded(field_count: usize, seed: u64) -> FormDef {
    let mut rng = Rng::new(seed);
    let form_id = FormId::from(rng.uuid());

    let mut sections = Vec::new();
    let mut remaining = field_count;
    let mut ordinal = 0i32;
    let mut kind_cursor = 0usize;

    while remaining > 0 {
        let columns = COLUMN_CYCLE[(ordinal as usize) % COLUMN_CYCLE.len()];
        let take = (rng.range(4, 12) as usize).min(remaining);
        let fields = build_fields(&mut rng, take, columns, &mut kind_cursor);

        sections.push(SectionDef {
            section_id: SectionId::from(rng.uuid()),
            title: format!("{} {}", rng.pick(SECTION_TITLES), ordinal + 1),
            ordinal,
            columns,
            // A collapsed section still contributes fields, and must contribute no tab
            // stops — the UI test depends on some form having one.
            default_collapsed: ordinal > 0 && rng.chance(15),
            fields,
        });

        remaining -= take;
        ordinal += 1;
    }

    // `FormDef::new` finalizes: assigns dense indices and clamps `col_span` to `columns`.
    FormDef::new(
        form_id,
        format!("Synthetic Form ({field_count} fields)"),
        sections,
    )
}

fn build_fields(
    rng: &mut Rng,
    count: usize,
    columns: u8,
    kind_cursor: &mut usize,
) -> Vec<SectionField> {
    let mut fields: Vec<SectionField> = Vec::with_capacity(count);

    for i in 0..count {
        let n = *kind_cursor;
        *kind_cursor += 1;
        let kind = build_kind(rng, n);
        let label = label_for(rng, &kind);

        fields.push(SectionField {
            idx: FieldIdx(0), // assigned by FormDef::finalize
            field: Arc::new(FieldDef {
                field_id: FieldId::from(rng.uuid()),
                key: format!("f{n:04}_{}", kind.tag()),
                kind,
            }),
            label,
            ordinal: i as i32,
            col_span: rng.range(1, columns as u64) as u8,
            required: rng.chance(20),
        });
    }

    // A multi-column section that never spans is not testing anything. Force at least one
    // full-width span; `finalize` will still clamp, which is the invariant under test.
    if columns > 1
        && !fields.iter().any(|f| f.col_span == columns)
        && let Some(first) = fields.first_mut()
    {
        first.col_span = columns;
    }

    fields
}

fn build_kind(rng: &mut Rng, n: usize) -> FieldKind {
    match n % KIND_COUNT {
        // R5
        0 => FieldKind::Text {
            max_len: Some(rng.range(24, 96) as u32),
        },
        // R6 — bounded so generated values are always in range, and scale varies so the
        // decimal path is exercised at 0, 1, and 2 places.
        1 => FieldKind::Numeric {
            min: Some(Decimal::ZERO),
            max: Some(Decimal::from(*rng.pick(&[100u32, 300, 999]))),
            scale: (n / KIND_COUNT % 3) as u8,
        },
        // R7
        2 => FieldKind::Date,
        // R8
        3 => FieldKind::Time,
        // R9
        4 => FieldKind::Radio {
            options: options_from(rng.pick(RADIO_SETS)),
        },
        // R10
        5 => FieldKind::Select {
            options: options_from(rng.pick(SELECT_SETS)),
            searchable: rng.chance(50),
        },
        // R11
        _ => FieldKind::Textarea {
            rows: rng.range(3, 6) as u16,
            max_len: Some(400),
        },
    }
}

fn options_from(set: &&[(&str, &str)]) -> Arc<[FieldOption]> {
    Arc::from(
        set.iter()
            .enumerate()
            .map(|(i, (code, label))| FieldOption {
                code: OptionCode::new(*code),
                label: (*label).to_string(),
                ordinal: i as i32,
            })
            .collect::<Vec<_>>(),
    )
}

fn label_for(rng: &mut Rng, kind: &FieldKind) -> String {
    let base = rng.pick(FIELD_LABELS);
    match kind {
        FieldKind::Numeric { .. } => format!("{base} ({})", rng.pick(UNITS)),
        _ => base.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn same_seed_produces_an_identical_form() {
        let a = synthetic_form_seeded(300, 12345);
        let b = synthetic_form_seeded(300, 12345);
        assert_eq!(a, b, "generation must be reproducible");
        assert_eq!(a.form_id, b.form_id, "even identities must be stable");
    }

    #[test]
    fn different_seeds_produce_different_forms() {
        let a = synthetic_form_seeded(300, 1);
        let b = synthetic_form_seeded(300, 2);
        assert_ne!(a, b);
    }

    #[test]
    fn field_count_is_exact() {
        for n in [1usize, 7, 50, 500] {
            assert_eq!(synthetic_form(n).field_count(), n, "requested {n}");
        }
    }

    #[test]
    fn all_seven_kinds_appear() {
        let form = synthetic_form(500);
        let tags: HashSet<&str> = form.iter_fields().map(|f| f.field.kind.tag()).collect();
        for expected in [
            "text", "numeric", "date", "time", "radio", "select", "textarea",
        ] {
            assert!(tags.contains(expected), "kind {expected} missing");
        }
        assert_eq!(tags.len(), KIND_COUNT);
    }

    #[test]
    fn seven_fields_is_enough_for_all_seven_kinds() {
        let form = synthetic_form(7);
        let tags: HashSet<&str> = form.iter_fields().map(|f| f.field.kind.tag()).collect();
        assert_eq!(tags.len(), KIND_COUNT, "kinds must cycle, not be sampled");
    }

    #[test]
    fn r12_columns_stay_in_range_and_vary() {
        let form = synthetic_form(500);
        let mut seen = HashSet::new();
        for s in &form.sections {
            assert!((1..=3).contains(&s.columns), "columns {}", s.columns);
            seen.insert(s.columns);
        }
        assert_eq!(
            seen,
            HashSet::from([1, 2, 3]),
            "all three tiers must appear"
        );
    }

    #[test]
    fn r12_col_span_never_exceeds_its_section() {
        let form = synthetic_form(500);
        for s in &form.sections {
            for f in &s.fields {
                assert!(
                    (1..=s.columns).contains(&f.col_span),
                    "col_span {} in a {}-column section",
                    f.col_span,
                    s.columns
                );
            }
        }
    }

    #[test]
    fn r12_multi_column_sections_actually_span() {
        let form = synthetic_form(500);
        let spanning = form
            .sections
            .iter()
            .filter(|s| s.columns > 1)
            .flat_map(|s| s.fields.iter())
            .filter(|f| f.col_span > 1)
            .count();
        assert!(spanning > 0, "no field spans more than one column");
    }

    #[test]
    fn field_ids_are_unique() {
        let form = synthetic_form(500);
        let ids: HashSet<FieldId> = form.iter_fields().map(|f| f.field.field_id).collect();
        assert_eq!(ids.len(), 500);
    }

    #[test]
    fn indices_are_dense_and_resolvable() {
        let form = synthetic_form(120);
        for (i, f) in form.iter_fields().enumerate() {
            assert_eq!(f.idx.0 as usize, i);
            assert_eq!(form.idx_of(f.field.field_id), Some(f.idx));
        }
    }
}
