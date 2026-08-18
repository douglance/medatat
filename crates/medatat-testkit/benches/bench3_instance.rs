//! Bench 3 (core half) — `FormInstance` construction and editing.
//!
//! `docs/07-TESTING.md` splits the open-case path into four spans:
//!
//! ```text
//! span "open_case"
//!   ├── span "store.load_values"      local SQLite read       (medatat-store)
//!   ├── span "core.build_instance"    FormInstance construction  ← measured here
//!   ├── span "ui.create_widgets"      300 Entity allocations  (medatat-ui)
//!   └── span "ui.first_paint"         to frame presented      (medatat-ui)
//! ```
//!
//! This file measures `core.build_instance` — the only span whose crate exists today. It
//! carries no CI gate of its own; the 50 ms gate belongs to the assembled Bench 3 once
//! `medatat-store` and `medatat-ui` land. The budget it has to leave room for is large:
//! instance construction should be well under 1 ms, so the other three spans own the rest.

use criterion::{BatchSize, Criterion, criterion_group, criterion_main};
use medatat_core::ids::{CaseId, CaseRev, FieldIdx};
use medatat_core::instance::FormInstance;
use medatat_core::value::Value;
use medatat_testkit::{synthetic_case, synthetic_form};
use std::hint::black_box;
use std::sync::Arc;

/// R13's stated scale: 500 fields on one case.
const FIELDS: usize = 500;
/// R14's stated scale: 300 changed fields in one editing burst.
const EDITS: usize = 300;

/// Measured at two sizes on purpose. `FormInstance::new` revalidates every field, and
/// `FormDef::field_at` resolves an index by linear scan, so construction is O(n²) in field
/// count. At R13's 500 fields that is comfortably inside the budget; the second data point
/// is what makes a regression at larger forms visible rather than inferred.
fn build_instance(c: &mut Criterion) {
    let mut group = c.benchmark_group("core.build_instance");
    for n in [FIELDS, FIELDS * 2] {
        let form = Arc::new(synthetic_form(n));
        let values = synthetic_case(&form, 1);
        group.bench_function(n.to_string(), |b| {
            b.iter_batched(
                || values.clone(),
                |values| {
                    black_box(FormInstance::new(
                        Arc::clone(&form),
                        CaseId::nil(),
                        CaseRev::ZERO,
                        values,
                    ))
                },
                // The instance owns n values plus two bitsets; a large batch would measure
                // the allocator's recycling behaviour rather than construction.
                BatchSize::SmallInput,
            );
        });
    }
    group.finish();
}

fn sequential_edits(c: &mut Criterion) {
    let form = Arc::new(synthetic_form(FIELDS));
    let values = synthetic_case(&form, 2);

    // Text fields only: `set` on a mismatched kind returns WrongType without exercising
    // the dirty-tracking path, which is what this measures.
    let text_pool: Vec<FieldIdx> = form
        .iter_fields()
        .filter(|f| matches!(f.field.kind.tag(), "text" | "textarea"))
        .map(|f| f.idx)
        .collect();
    let text_indices: Vec<FieldIdx> = text_pool.iter().copied().cycle().take(EDITS).collect();
    assert_eq!(
        text_indices.len(),
        EDITS,
        "the fixture must supply enough text fields"
    );

    c.bench_function("core.set/300_sequential", |b| {
        b.iter_batched(
            || {
                FormInstance::new(
                    Arc::clone(&form),
                    CaseId::nil(),
                    CaseRev::ZERO,
                    values.clone(),
                )
            },
            |mut inst| {
                for (n, idx) in text_indices.iter().enumerate() {
                    black_box(inst.set(*idx, Value::Text(format!("edit {n}"))));
                }
                black_box(inst.dirty_count())
            },
            BatchSize::SmallInput,
        );
    });
}

/// The anti-quadratic guard, measured rather than asserted: `pending` must cost O(dirty),
/// so one edit on a 500-field form must not get slower as the form grows.
fn pending_after_one_edit(c: &mut Criterion) {
    let form = Arc::new(synthetic_form(FIELDS));
    let values = synthetic_case(&form, 3);
    let mut inst = FormInstance::new(Arc::clone(&form), CaseId::nil(), CaseRev::ZERO, values);
    inst.set(FieldIdx(0), Value::Text("dirty".into()));

    c.bench_function("core.pending/1_of_500", |b| {
        b.iter(|| black_box(inst.pending().count()));
    });
}

criterion_group!(
    benches,
    build_instance,
    sequential_edits,
    pending_after_one_edit
);
criterion_main!(benches);
