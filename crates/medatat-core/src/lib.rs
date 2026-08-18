//! medatat domain core.
//!
//! Types, validation, and the live form model. **No I/O of any kind** — this crate compiles
//! to `wasm32-unknown-unknown` and is shared verbatim between the desktop client and the
//! Cloudflare Worker.
//!
//! That sharing buys three things, and they are the three that would otherwise silently
//! corrupt clinical data:
//!
//! 1. One [`validate`] engine, called by the keystroke handler *and* the server write path.
//! 2. One [`def::FieldKind::value_column`] mapping, used by every query builder.
//! 3. Identical [`value::parse_time_24`] and decimal semantics on both sides.
//!
//! See `docs/11-CRATE-GUIDE.md`.

pub mod builder;
pub mod def;
pub mod error;
pub mod ids;
pub mod instance;
pub mod layout;
pub mod validate;
pub mod value;
pub mod view;
pub mod wire;

pub use builder::{EditError, FieldEdit, classify, plan_column_change, validate_form};
pub use def::{FieldDef, FieldKind, FieldOption, FormDef, SectionDef, SectionField};
pub use error::{CoreError, FieldError, ValidationError};
pub use ids::{
    ActorId, CaseId, CaseRev, ConfigRev, FieldId, FieldIdx, FormId, OptionCode, SectionId,
};
pub use instance::FormInstance;
pub use layout::{clamp_col_span, col_span_is_valid, effective_columns};
pub use validate::{validate, validate_placement};
pub use value::{
    Value, ValueColumn, format_date, format_decimal, format_time_24, parse_date, parse_decimal,
    parse_time_24,
};
pub use view::{WidgetKind, WidgetSpec, focus_order, search_fields, widget_spec};
