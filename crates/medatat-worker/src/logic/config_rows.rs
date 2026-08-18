//! D1 rows in, `medatat_core` types out.
//!
//! The D1 tables in `migrations/0001_init.sql` are flat and the wire types are a tree, so
//! something has to join them. Doing it here, over plain row structs, means the join is
//! tested natively and the D1 adapter in `store/d1.rs` stays a set of queries.
//!
//! The `field.config` column is the kind-specific JSON documented in `docs/03-API.md`.
//! [`kind_to_row`] and [`kind_from_row`] are inverses, and a round-trip test says so.

use crate::error::{LogicError, LogicResult};
use crate::logic::case_store::FieldLookup;
use medatat_core::def::{FieldDef, FieldKind, FieldOption, FormDef, SectionDef, SectionField};
use medatat_core::ids::{ConfigRev, FieldId, FieldIdx, FormId, OptionCode, SectionId};
use medatat_core::value::parse_decimal;
use medatat_core::wire::ConfigDelta;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FormRow {
    pub form_id: String,
    pub key: String,
    pub name: String,
    #[serde(default)]
    pub archived_at: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SectionRow {
    pub section_id: String,
    pub form_id: String,
    pub name: String,
    pub ordinal: i64,
    pub columns: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FieldRow {
    pub field_id: String,
    pub key: String,
    pub kind: String,
    pub config: String,
    #[serde(default)]
    pub archived_at: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OptionRow {
    pub field_id: String,
    pub code: String,
    pub label: String,
    pub ordinal: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlacementRow {
    pub section_id: String,
    pub field_id: String,
    pub ordinal: i64,
    pub col_span: i64,
    pub label: String,
    pub required: i64,
}

/// The `field.config` column. Every key is optional so one struct covers all seven kinds
/// and an older row missing a key still loads.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FieldConfigJson {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_len: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rows: Option<u16>,
    /// Decimal **strings**, never JSON numbers — a float here is the corruption R6 exists
    /// to prevent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scale: Option<u8>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub searchable: Option<bool>,
}

/// A kind split into the two columns D1 stores it in: the `kind` tag and the `config` JSON.
/// Options live in their own table and are not part of `config`.
pub fn kind_to_row(kind: &FieldKind) -> LogicResult<(&'static str, String)> {
    let cfg = match kind {
        FieldKind::Text { max_len } => FieldConfigJson {
            max_len: *max_len,
            ..Default::default()
        },
        FieldKind::Textarea { rows, max_len } => FieldConfigJson {
            rows: Some(*rows),
            max_len: *max_len,
            ..Default::default()
        },
        FieldKind::Numeric { min, max, scale } => FieldConfigJson {
            min: min.map(|d| d.to_string()),
            max: max.map(|d| d.to_string()),
            scale: Some(*scale),
            ..Default::default()
        },
        FieldKind::Date | FieldKind::Time | FieldKind::Radio { .. } => FieldConfigJson::default(),
        FieldKind::Select { searchable, .. } => FieldConfigJson {
            searchable: Some(*searchable),
            ..Default::default()
        },
    };
    let json = serde_json::to_string(&cfg)
        .map_err(|e| LogicError::Internal(format!("field config encode: {e}")))?;
    Ok((kind.tag(), json))
}

/// The inverse of [`kind_to_row`]. `options` come from `field_option`, already ordered.
pub fn kind_from_row(
    tag: &str,
    config: &str,
    options: Arc<[FieldOption]>,
) -> LogicResult<FieldKind> {
    let cfg: FieldConfigJson = if config.trim().is_empty() {
        FieldConfigJson::default()
    } else {
        serde_json::from_str(config)
            .map_err(|e| LogicError::Storage(format!("field config decode: {e}")))?
    };
    let decimal = |o: &Option<String>, which: &str| match o {
        None => Ok(None),
        Some(s) => parse_decimal(s).map(Some).map_err(|_| {
            LogicError::Storage(format!("field config {which} is not a decimal: {s}"))
        }),
    };
    Ok(match tag {
        "text" => FieldKind::Text {
            max_len: cfg.max_len,
        },
        "textarea" => FieldKind::Textarea {
            rows: cfg.rows.unwrap_or(4),
            max_len: cfg.max_len,
        },
        "numeric" => FieldKind::Numeric {
            min: decimal(&cfg.min, "min")?,
            max: decimal(&cfg.max, "max")?,
            scale: cfg.scale.unwrap_or(0),
        },
        "date" => FieldKind::Date,
        "time" => FieldKind::Time,
        "radio" => FieldKind::Radio { options },
        "select" => FieldKind::Select {
            options,
            searchable: cfg.searchable.unwrap_or(false),
        },
        other => {
            return Err(LogicError::Storage(format!(
                "unknown field kind tag {other:?}"
            )));
        }
    })
}

fn parse_id<T, F, E>(raw: &str, what: &str, f: F) -> LogicResult<T>
where
    F: Fn(&str) -> Result<T, E>,
    E: std::fmt::Display,
{
    f(raw).map_err(|e| LogicError::Storage(format!("bad {what} {raw:?}: {e}")))
}

fn options_by_field(rows: &[OptionRow]) -> LogicResult<HashMap<FieldId, Arc<[FieldOption]>>> {
    let mut grouped: HashMap<FieldId, Vec<FieldOption>> = HashMap::new();
    for r in rows {
        let id = parse_id(&r.field_id, "field_id", FieldId::parse)?;
        grouped.entry(id).or_default().push(FieldOption {
            code: OptionCode::new(r.code.clone()),
            label: r.label.clone(),
            ordinal: r.ordinal as i32,
        });
    }
    Ok(grouped
        .into_iter()
        .map(|(id, mut opts)| {
            opts.sort_by_key(|o| o.ordinal);
            (id, Arc::from(opts))
        })
        .collect())
}

/// Every field definition, by id. This is what the write path validates against, and what
/// the DO needs to know which storage column a value belongs in.
pub fn field_defs(fields: &[FieldRow], options: &[OptionRow]) -> LogicResult<Vec<FieldDef>> {
    let by_field = options_by_field(options)?;
    fields
        .iter()
        .map(|f| {
            let field_id = parse_id(&f.field_id, "field_id", FieldId::parse)?;
            let opts = by_field
                .get(&field_id)
                .cloned()
                .unwrap_or_else(|| Arc::from(vec![]));
            Ok(FieldDef {
                field_id,
                key: f.key.clone(),
                kind: kind_from_row(&f.kind, &f.config, opts)?,
            })
        })
        .collect()
}

pub fn field_lookup(fields: &[FieldRow], options: &[OptionRow]) -> LogicResult<FieldLookup> {
    Ok(FieldLookup::from_defs(field_defs(fields, options)?))
}

/// Join five flat tables into the `GET /config` body.
///
/// Archived forms and fields are dropped: a client that cannot see a definition cannot be
/// asked to render it. Values behind an archived field are untouched — archiving is a
/// configuration act, never a data one.
pub fn assemble_config(
    config_rev: ConfigRev,
    forms: &[FormRow],
    sections: &[SectionRow],
    fields: &[FieldRow],
    options: &[OptionRow],
    placements: &[PlacementRow],
) -> LogicResult<ConfigDelta> {
    let defs: HashMap<FieldId, Arc<FieldDef>> = field_defs(fields, options)?
        .into_iter()
        .map(|d| (d.field_id, Arc::new(d)))
        .collect();
    // Archived fields drop out of the shipped configuration. An id that is not in `field`
    // at all is a different thing entirely and must surface as an error below.
    let archived: std::collections::HashSet<&str> = fields
        .iter()
        .filter(|f| f.archived_at.is_some())
        .map(|f| f.field_id.as_str())
        .collect();

    let mut placements_by_section: HashMap<&str, Vec<&PlacementRow>> = HashMap::new();
    for p in placements {
        if !archived.contains(p.field_id.as_str()) {
            placements_by_section
                .entry(p.section_id.as_str())
                .or_default()
                .push(p);
        }
    }

    let mut sections_by_form: HashMap<&str, Vec<&SectionRow>> = HashMap::new();
    for s in sections {
        sections_by_form
            .entry(s.form_id.as_str())
            .or_default()
            .push(s);
    }

    let mut out = Vec::new();
    for form in forms.iter().filter(|f| f.archived_at.is_none()) {
        let form_id = parse_id(&form.form_id, "form_id", FormId::parse)?;
        let mut section_defs = Vec::new();
        for s in sections_by_form
            .remove(form.form_id.as_str())
            .unwrap_or_default()
        {
            let mut section_fields = Vec::new();
            for p in placements_by_section
                .get(s.section_id.as_str())
                .cloned()
                .unwrap_or_default()
            {
                let field_id = parse_id(&p.field_id, "field_id", FieldId::parse)?;
                let def = defs
                    .get(&field_id)
                    .ok_or(LogicError::UnknownField(field_id))?;
                section_fields.push(SectionField {
                    idx: FieldIdx(0),
                    field: Arc::clone(def),
                    label: p.label.clone(),
                    ordinal: p.ordinal as i32,
                    col_span: p.col_span.clamp(1, 3) as u8,
                    required: p.required != 0,
                });
            }
            section_defs.push(SectionDef {
                section_id: parse_id(&s.section_id, "section_id", SectionId::parse)?,
                title: s.name.clone(),
                ordinal: s.ordinal as i32,
                columns: s.columns.clamp(1, 3) as u8,
                default_collapsed: false,
                fields: section_fields,
            });
        }
        // `FormDef::new` finalises: sections and fields sort by ordinal, dense indices are
        // assigned, and R12 clamps every `col_span` to its section's `columns`.
        out.push(FormDef::new(form_id, form.name.clone(), section_defs));
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(ConfigDelta {
        config_rev,
        forms: out,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn form_row(id: &str, name: &str, archived: bool) -> FormRow {
        FormRow {
            form_id: id.into(),
            key: name.to_ascii_lowercase(),
            name: name.into(),
            archived_at: archived.then(|| "2026-08-17T00:00:00Z".to_string()),
        }
    }

    fn section_row(id: &str, form: &str, ordinal: i64, columns: i64) -> SectionRow {
        SectionRow {
            section_id: id.into(),
            form_id: form.into(),
            name: format!("Section {ordinal}"),
            ordinal,
            columns,
        }
    }

    fn field_row(id: &str, kind: &str, config: &str) -> FieldRow {
        FieldRow {
            field_id: id.into(),
            key: format!("k-{kind}"),
            kind: kind.into(),
            config: config.into(),
            archived_at: None,
        }
    }

    fn placement(section: &str, field: &str, ordinal: i64, col_span: i64) -> PlacementRow {
        PlacementRow {
            section_id: section.into(),
            field_id: field.into(),
            ordinal,
            col_span,
            label: format!("Field {ordinal}"),
            required: 0,
        }
    }

    fn uuid(n: u8) -> String {
        format!("00000000-0000-4000-8000-0000000000{n:02x}")
    }

    #[test]
    fn every_kind_round_trips_through_its_two_columns() {
        let opts: Arc<[FieldOption]> = Arc::from(vec![FieldOption {
            code: OptionCode::new("M"),
            label: "Male".into(),
            ordinal: 0,
        }]);
        for kind in [
            FieldKind::Text { max_len: Some(255) },
            FieldKind::Textarea {
                rows: 4,
                max_len: Some(4000),
            },
            FieldKind::Numeric {
                min: Some(parse_decimal("0").unwrap()),
                max: Some(parse_decimal("300").unwrap()),
                scale: 2,
            },
            FieldKind::Date,
            FieldKind::Time,
            FieldKind::Radio {
                options: opts.clone(),
            },
            FieldKind::Select {
                options: opts.clone(),
                searchable: true,
            },
        ] {
            let (tag, config) = kind_to_row(&kind).unwrap();
            let back = kind_from_row(tag, &config, opts.clone()).unwrap();
            assert_eq!(back, kind, "round trip failed for {tag}");
        }
    }

    #[test]
    fn r6_numeric_bounds_survive_as_decimal_strings() {
        let kind = FieldKind::Numeric {
            min: Some(parse_decimal("0.10").unwrap()),
            max: Some(parse_decimal("999.95").unwrap()),
            scale: 2,
        };
        let (_, config) = kind_to_row(&kind).unwrap();
        assert!(
            config.contains("\"0.10\"") && config.contains("\"999.95\""),
            "bounds must be JSON strings, not floats: {config}"
        );
    }

    #[test]
    fn an_unknown_kind_tag_is_a_storage_error_not_a_panic() {
        assert!(kind_from_row("colour", "{}", Arc::from(vec![])).is_err());
    }

    #[test]
    fn an_empty_config_column_loads_as_defaults() {
        let k = kind_from_row("text", "", Arc::from(vec![])).unwrap();
        assert_eq!(k, FieldKind::Text { max_len: None });
    }

    #[test]
    fn options_arrive_in_ordinal_order() {
        let f = uuid(1);
        let rows = vec![
            OptionRow {
                field_id: f.clone(),
                code: "B".into(),
                label: "b".into(),
                ordinal: 1,
            },
            OptionRow {
                field_id: f.clone(),
                code: "A".into(),
                label: "a".into(),
                ordinal: 0,
            },
        ];
        let defs = field_defs(&[field_row(&f, "radio", "{}")], &rows).unwrap();
        let codes: Vec<&str> = defs[0]
            .kind
            .options()
            .unwrap()
            .iter()
            .map(|o| o.code.as_str())
            .collect();
        assert_eq!(codes, vec!["A", "B"]);
    }

    #[test]
    fn r12_assembly_clamps_col_span_to_section_columns() {
        let (form, section, field) = (uuid(1), uuid(2), uuid(3));
        let delta = assemble_config(
            ConfigRev(7),
            &[form_row(&form, "Intake", false)],
            &[section_row(&section, &form, 0, 2)],
            &[field_row(&field, "text", "{}")],
            &[],
            &[placement(&section, &field, 0, 3)],
        )
        .unwrap();
        assert_eq!(delta.config_rev, ConfigRev(7));
        let span = delta.forms[0].sections[0].fields[0].col_span;
        assert_eq!(span, 2, "a 3-span in a 2-column section must clamp");
    }

    #[test]
    fn archived_forms_and_fields_are_not_shipped_to_clients() {
        let (live_form, dead_form, section, field, dead_field) =
            (uuid(1), uuid(2), uuid(3), uuid(4), uuid(5));
        let mut archived = field_row(&dead_field, "text", "{}");
        archived.archived_at = Some("2026-08-17T00:00:00Z".into());

        let delta = assemble_config(
            ConfigRev(1),
            &[
                form_row(&live_form, "Live", false),
                form_row(&dead_form, "Dead", true),
            ],
            &[section_row(&section, &live_form, 0, 1)],
            &[field_row(&field, "text", "{}"), archived],
            &[],
            &[
                placement(&section, &field, 0, 1),
                placement(&section, &dead_field, 1, 1),
            ],
        )
        .unwrap();

        assert_eq!(delta.forms.len(), 1, "the archived form must be dropped");
        assert_eq!(
            delta.forms[0].field_count(),
            1,
            "the archived field must be dropped from its placement"
        );
    }

    #[test]
    fn sections_and_fields_come_back_in_ordinal_order() {
        let (form, s0, s1, f0, f1) = (uuid(1), uuid(2), uuid(3), uuid(4), uuid(5));
        let delta = assemble_config(
            ConfigRev(1),
            &[form_row(&form, "Intake", false)],
            &[section_row(&s1, &form, 1, 1), section_row(&s0, &form, 0, 1)],
            &[field_row(&f0, "text", "{}"), field_row(&f1, "date", "{}")],
            &[],
            &[
                placement(&s0, &f1, 1, 1),
                placement(&s0, &f0, 0, 1),
                placement(&s1, &f0, 0, 1),
            ],
        )
        .unwrap();
        let form = &delta.forms[0];
        assert_eq!(form.sections[0].ordinal, 0);
        assert_eq!(form.sections[1].ordinal, 1);
        let first = &form.sections[0].fields;
        assert_eq!(first[0].ordinal, 0);
        assert_eq!(first[1].ordinal, 1);
    }

    #[test]
    fn a_placement_pointing_at_no_field_is_reported_not_skipped() {
        let (form, section, missing) = (uuid(1), uuid(2), uuid(9));
        let err = assemble_config(
            ConfigRev(1),
            &[form_row(&form, "Intake", false)],
            &[section_row(&section, &form, 0, 1)],
            &[field_row(&missing, "text", "{}")],
            &[],
            &[placement(&section, &uuid(8), 0, 1)],
        );
        assert!(
            matches!(err, Err(LogicError::UnknownField(_))),
            "a dangling placement must surface, not silently vanish"
        );
    }

    #[test]
    fn the_field_lookup_covers_every_configured_field() {
        let (a, b) = (uuid(1), uuid(2));
        let lookup = field_lookup(
            &[
                field_row(&a, "time", "{}"),
                field_row(&b, "numeric", r#"{"scale":2}"#),
            ],
            &[],
        )
        .unwrap();
        assert_eq!(lookup.len(), 2);
        assert_eq!(
            lookup.get(FieldId::parse(&a).unwrap()).unwrap().kind,
            FieldKind::Time
        );
    }
}
