//! Pure presentation logic: what a field should look like, and where focus goes.
//!
//! This lives in `medatat-core` rather than `medatat-ui` deliberately. The design goal is
//! to keep the GUI test budget at three tests by testing rendering "one level below
//! pixels" — but that only works if the presentation decisions are reachable without
//! linking gpui at all. Putting them here makes `widget_spec` and `focus_order` ordinary
//! unit-testable functions, and gives R5–R12 full regression coverage with no window.
//!
//! `medatat-ui` maps a [`WidgetSpec`] onto gpui-component widgets and does nothing else.

use crate::def::{FieldKind, FormDef, SectionField};
use crate::error::ValidationError;
use crate::ids::FieldIdx;
use crate::instance::FormInstance;
use crate::value::{Value, format_decimal_scaled};

/// Which concrete widget renders a field (R5–R11).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WidgetKind {
    Text,
    Numeric,
    Date,
    /// 24-hour masked input. Hand-built — gpui-component has no TimePicker (R8).
    Time,
    Radio,
    Select,
    Textarea,
}

impl WidgetKind {
    pub fn of(kind: &FieldKind) -> Self {
        match kind {
            FieldKind::Text { .. } => WidgetKind::Text,
            FieldKind::Numeric { .. } => WidgetKind::Numeric,
            FieldKind::Date => WidgetKind::Date,
            FieldKind::Time => WidgetKind::Time,
            FieldKind::Radio { .. } => WidgetKind::Radio,
            FieldKind::Select { .. } => WidgetKind::Select,
            FieldKind::Textarea { .. } => WidgetKind::Textarea,
        }
    }

    /// Whether this widget needs its own gpui `Entity`. Radio groups and non-searchable
    /// selects do not, which materially cuts the entity count on a large form.
    pub fn needs_entity(self) -> bool {
        !matches!(self, WidgetKind::Radio)
    }
}

/// A complete, renderer-agnostic description of one field's presentation.
#[derive(Debug, Clone, PartialEq)]
pub struct WidgetSpec {
    pub idx: FieldIdx,
    pub kind: WidgetKind,
    pub label: String,
    /// Exactly what the input should display. Empty for a null value.
    pub value: String,
    pub error: Option<ValidationError>,
    pub required: bool,
    /// 1..=3, already clamped to the effective column count (R12).
    pub col_span: u8,
    pub max_len: Option<u32>,
    pub rows: u16,
    /// `(code, label)` pairs for radio and select (R9, R10).
    pub options: Vec<(String, String)>,
    pub searchable: bool,
}

/// Builds the presentation for one field.
///
/// `effective_cols` is the section's column count *after* responsive degradation, so the
/// span can never exceed what is actually rendered.
pub fn widget_spec(sf: &SectionField, inst: &FormInstance, effective_cols: u8) -> WidgetSpec {
    let kind = &sf.field.kind;
    let value = inst.get(sf.idx);

    WidgetSpec {
        idx: sf.idx,
        kind: WidgetKind::of(kind),
        label: sf.label.clone(),
        value: display_value(kind, value),
        error: inst.error(sf.idx).cloned(),
        required: sf.required,
        col_span: crate::layout::clamp_col_span(sf.col_span, effective_cols),
        max_len: match kind {
            FieldKind::Text { max_len } | FieldKind::Textarea { max_len, .. } => *max_len,
            // Five characters is exactly "HH:MM" — the mask is a length limit, not a regex.
            FieldKind::Time => Some(5),
            _ => None,
        },
        rows: match kind {
            FieldKind::Textarea { rows, .. } => *rows,
            _ => 1,
        },
        options: kind
            .options()
            .map(|o| {
                o.iter()
                    .map(|x| (x.code.0.clone(), x.label.clone()))
                    .collect()
            })
            .unwrap_or_default(),
        searchable: matches!(
            kind,
            FieldKind::Select {
                searchable: true,
                ..
            }
        ),
    }
}

/// The canonical display string for a value.
///
/// Numeric respects the field's configured scale so `1.5` shows as `1.50` on a 2-decimal
/// field. Everything else uses the storage encoding, which is already canonical — notably
/// time is always `HH:MM`, never locale-formatted.
fn display_value(kind: &FieldKind, value: &Value) -> String {
    match (kind, value) {
        (FieldKind::Numeric { scale, .. }, Value::Num(d)) if *scale > 0 => {
            format_decimal_scaled(*d, *scale)
        }
        _ => value.to_display_string(),
    }
}

/// Tab order: section order, then field order, skipping collapsed sections.
///
/// A collapsed section must contribute no focus stops — tabbing into something invisible
/// is the fastest way to make a keyboard-driven form feel broken.
pub fn focus_order(def: &FormDef, collapsed: &[bool]) -> Vec<FieldIdx> {
    let mut out = Vec::with_capacity(def.field_count());
    for (i, section) in def.sections.iter().enumerate() {
        if collapsed.get(i).copied().unwrap_or(false) {
            continue;
        }
        out.extend(section.fields.iter().map(|f| f.idx));
    }
    out
}

/// Fuzzy subsequence match for the `Cmd/Ctrl-F` field palette.
/// Case-insensitive; returns a score where lower is a tighter match.
pub fn fuzzy_score(needle: &str, haystack: &str) -> Option<u32> {
    if needle.is_empty() {
        return Some(u32::MAX);
    }
    let h: Vec<char> = haystack.to_lowercase().chars().collect();
    let mut score = 0u32;
    let mut hi = 0usize;
    let mut last = None::<usize>;

    for nc in needle.to_lowercase().chars() {
        let found = h[hi..].iter().position(|&c| c == nc)? + hi;
        if let Some(prev) = last {
            score += (found - prev) as u32; // reward adjacency
        } else {
            score += found as u32; // reward matching near the start
        }
        last = Some(found);
        hi = found + 1;
    }
    Some(score)
}

/// Fields whose label fuzzy-matches, best first.
pub fn search_fields(def: &FormDef, needle: &str) -> Vec<FieldIdx> {
    let mut hits: Vec<(u32, FieldIdx)> = def
        .iter_fields()
        .filter_map(|f| fuzzy_score(needle, &f.label).map(|s| (s, f.idx)))
        .collect();
    hits.sort_by_key(|(s, idx)| (*s, idx.0));
    hits.into_iter().map(|(_, idx)| idx).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::def::{FieldDef, FieldOption, SectionDef};
    use crate::ids::{CaseId, CaseRev, FieldId, FormId, OptionCode, SectionId};
    use crate::value::{parse_date, parse_decimal, parse_time_24};
    use std::sync::Arc;

    fn sf(label: &str, kind: FieldKind, col_span: u8, required: bool) -> SectionField {
        SectionField {
            idx: FieldIdx(0),
            field: Arc::new(FieldDef {
                field_id: FieldId::new(),
                key: label.into(),
                kind,
            }),
            label: label.into(),
            ordinal: 0,
            col_span,
            required,
        }
    }

    fn form_of(columns: u8, fields: Vec<SectionField>) -> Arc<FormDef> {
        let fields = fields
            .into_iter()
            .enumerate()
            .map(|(i, mut f)| {
                f.ordinal = i as i32;
                f
            })
            .collect();
        Arc::new(FormDef::new(
            FormId::new(),
            "f",
            vec![SectionDef {
                section_id: SectionId::new(),
                title: "s".into(),
                ordinal: 0,
                columns,
                default_collapsed: false,
                fields,
            }],
        ))
    }

    fn opts() -> Arc<[FieldOption]> {
        Arc::from(vec![
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
        ])
    }

    #[test]
    fn r5_to_r11_every_kind_maps_to_a_widget() {
        let def = form_of(
            1,
            vec![
                sf("t", FieldKind::Text { max_len: Some(9) }, 1, false),
                sf(
                    "n",
                    FieldKind::Numeric {
                        min: None,
                        max: None,
                        scale: 2,
                    },
                    1,
                    false,
                ),
                sf("d", FieldKind::Date, 1, false),
                sf("h", FieldKind::Time, 1, false),
                sf("r", FieldKind::Radio { options: opts() }, 1, false),
                sf(
                    "s",
                    FieldKind::Select {
                        options: opts(),
                        searchable: true,
                    },
                    1,
                    false,
                ),
                sf(
                    "a",
                    FieldKind::Textarea {
                        rows: 6,
                        max_len: None,
                    },
                    1,
                    false,
                ),
            ],
        );
        let inst = FormInstance::new(Arc::clone(&def), CaseId::new(), CaseRev::ZERO, []);
        let kinds: Vec<WidgetKind> = def
            .iter_fields()
            .map(|f| widget_spec(f, &inst, 1).kind)
            .collect();
        assert_eq!(
            kinds,
            vec![
                WidgetKind::Text,
                WidgetKind::Numeric,
                WidgetKind::Date,
                WidgetKind::Time,
                WidgetKind::Radio,
                WidgetKind::Select,
                WidgetKind::Textarea,
            ]
        );
    }

    #[test]
    fn r8_time_displays_as_24hr_and_is_length_capped() {
        let def = form_of(1, vec![sf("t", FieldKind::Time, 1, false)]);
        let id = def.iter_fields().next().unwrap().field.field_id;
        let inst = FormInstance::new(
            Arc::clone(&def),
            CaseId::new(),
            CaseRev::ZERO,
            [(id, Value::Time(parse_time_24("9:5").unwrap()))],
        );
        let spec = widget_spec(def.iter_fields().next().unwrap(), &inst, 1);
        assert_eq!(spec.value, "09:05");
        assert_eq!(spec.max_len, Some(5));
    }

    #[test]
    fn r6_numeric_renders_at_configured_scale() {
        let def = form_of(
            1,
            vec![sf(
                "n",
                FieldKind::Numeric {
                    min: None,
                    max: None,
                    scale: 2,
                },
                1,
                false,
            )],
        );
        let id = def.iter_fields().next().unwrap().field.field_id;
        let inst = FormInstance::new(
            Arc::clone(&def),
            CaseId::new(),
            CaseRev::ZERO,
            [(id, Value::Num(parse_decimal("1.5").unwrap()))],
        );
        assert_eq!(
            widget_spec(def.iter_fields().next().unwrap(), &inst, 1).value,
            "1.50"
        );
    }

    #[test]
    fn r7_date_renders_iso() {
        let def = form_of(1, vec![sf("d", FieldKind::Date, 1, false)]);
        let id = def.iter_fields().next().unwrap().field.field_id;
        let inst = FormInstance::new(
            Arc::clone(&def),
            CaseId::new(),
            CaseRev::ZERO,
            [(id, Value::Date(parse_date("2026-08-17").unwrap()))],
        );
        assert_eq!(
            widget_spec(def.iter_fields().next().unwrap(), &inst, 1).value,
            "2026-08-17"
        );
    }

    #[test]
    fn r9_r10_options_are_carried_through() {
        let def = form_of(
            1,
            vec![sf("r", FieldKind::Radio { options: opts() }, 1, false)],
        );
        let inst = FormInstance::new(Arc::clone(&def), CaseId::new(), CaseRev::ZERO, []);
        let spec = widget_spec(def.iter_fields().next().unwrap(), &inst, 1);
        assert_eq!(
            spec.options,
            vec![("M".into(), "Male".into()), ("F".into(), "Female".into())]
        );
    }

    #[test]
    fn r12_span_clamps_to_effective_columns() {
        let def = form_of(
            3,
            vec![sf("a", FieldKind::Text { max_len: None }, 3, false)],
        );
        let inst = FormInstance::new(Arc::clone(&def), CaseId::new(), CaseRev::ZERO, []);
        let f = def.iter_fields().next().unwrap();
        assert_eq!(widget_spec(f, &inst, 3).col_span, 3);
        assert_eq!(
            widget_spec(f, &inst, 2).col_span,
            2,
            "narrowed window must clamp"
        );
        assert_eq!(widget_spec(f, &inst, 1).col_span, 1);
    }

    #[test]
    fn null_renders_as_empty_not_the_word_null() {
        let def = form_of(
            1,
            vec![sf("t", FieldKind::Text { max_len: None }, 1, false)],
        );
        let inst = FormInstance::new(Arc::clone(&def), CaseId::new(), CaseRev::ZERO, []);
        assert_eq!(
            widget_spec(def.iter_fields().next().unwrap(), &inst, 1).value,
            ""
        );
    }

    #[test]
    fn radio_needs_no_entity() {
        assert!(!WidgetKind::Radio.needs_entity());
        assert!(WidgetKind::Text.needs_entity());
    }

    #[test]
    fn collapsed_sections_contribute_no_focus_stops() {
        let two = FormDef::new(
            FormId::new(),
            "f",
            vec![
                SectionDef {
                    section_id: SectionId::new(),
                    title: "a".into(),
                    ordinal: 0,
                    columns: 1,
                    default_collapsed: false,
                    fields: vec![sf("a1", FieldKind::Date, 1, false)],
                },
                SectionDef {
                    section_id: SectionId::new(),
                    title: "b".into(),
                    ordinal: 1,
                    columns: 1,
                    default_collapsed: false,
                    fields: vec![sf("b1", FieldKind::Date, 1, false)],
                },
            ],
        );
        assert_eq!(focus_order(&two, &[false, false]).len(), 2);
        assert_eq!(focus_order(&two, &[false, true]), vec![FieldIdx(0)]);
        assert_eq!(focus_order(&two, &[true, true]).len(), 0);
    }

    #[test]
    fn fuzzy_search_ranks_tighter_matches_first() {
        assert!(fuzzy_score("dob", "Date of birth").is_some());
        assert!(fuzzy_score("xyz", "Date of birth").is_none());
        let tight = fuzzy_score("dat", "Date").unwrap();
        let loose = fuzzy_score("dat", "Discharge admit type").unwrap();
        assert!(tight < loose, "tight={tight} loose={loose}");
    }

    #[test]
    fn search_returns_best_match_first() {
        let def = form_of(
            1,
            vec![
                sf("Discharge admit type", FieldKind::Date, 1, false),
                sf("Date", FieldKind::Date, 1, false),
            ],
        );
        assert_eq!(search_fields(&def, "dat").first(), Some(&FieldIdx(1)));
    }
}
