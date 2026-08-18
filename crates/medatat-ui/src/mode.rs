//! Runtime versus design rendering (`docs/06-FORM-BUILDER.md`).
//!
//! There is **one** renderer. `FormView` emits the same element tree in both modes; design
//! mode changes only hit-testing, chrome, and whether the widgets accept input. That is what
//! makes the builder WYSIWYG for free, and it is why a layout bug cannot appear in the
//! builder but not the runtime — there is no second layout implementation to disagree with.
//!
//! If you find yourself adding a branch here that emits *different elements* rather than
//! different decoration, that is the signal you are building the second renderer the spec
//! forbids.

use medatat_core::FieldIdx;

/// What the inspector is looking at. Multi-select is out of scope.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Selection {
    /// Index into `FormDef::sections`.
    Section(usize),
    Field(FieldIdx),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum RenderMode {
    /// What an abstractor sees. Also what "Preview as abstractor" flips back to.
    #[default]
    Runtime,
    /// What a coordinator sees in the builder.
    ///
    /// The spec's shape also carries a `drop_target` for drag-reorder. It is deliberately
    /// absent: `gpui-component` has no drag primitive, drag is explicitly *not* an M5 gate,
    /// and an always-`None` field would be dead weight until it exists.
    Design { selected: Option<Selection> },
}

impl RenderMode {
    pub fn is_design(self) -> bool {
        matches!(self, RenderMode::Design { .. })
    }

    pub fn selected(self) -> Option<Selection> {
        match self {
            RenderMode::Design { selected } => selected,
            RenderMode::Runtime => None,
        }
    }

    pub fn selects_field(self, idx: FieldIdx) -> bool {
        self.selected() == Some(Selection::Field(idx))
    }

    pub fn selects_section(self, section: usize) -> bool {
        self.selected() == Some(Selection::Section(section))
    }
}
