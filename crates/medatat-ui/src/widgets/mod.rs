//! The only place `gpui-component` is called.
//!
//! Everything above this module works in terms of [`medatat_core::WidgetSpec`]. That keeps
//! an upstream breaking change a one-file fix rather than a hundred-site edit, which
//! matters because GPUI is self-declared pre-1.0 and pinned to a git SHA of a code
//! editor's internals. See `docs/adr/0003-gpui-component.md`.
//!
//! The containment is not just about imports. Widget *events* are translated here too —
//! [`subscribe`] hands the form layer a [`WidgetChange`], never an `InputEvent`, a
//! `SelectEvent`, or a `DatePickerEvent` — so `form/view.rs` names no upstream type at all.

pub mod time_input;

use crate::mode::RenderMode;
use gpui::{
    AnyElement, AnyView, App, AppContext as _, Context, Entity, FocusHandle, Focusable as _,
    InteractiveElement as _, IntoElement, ParentElement as _, Render, SharedString,
    StatefulInteractiveElement as _, Styled as _, Subscription, Window, WindowOptions, div, px,
};
use gpui_component::date_picker::{DatePicker, DatePickerEvent, DatePickerState};
use gpui_component::form::{Field, field, v_form};
use gpui_component::input::{Input, InputEvent, InputState, NumberInput, Textarea, TextareaState};
use gpui_component::radio::{Radio, RadioGroup};
use gpui_component::searchable_list::SearchableListItem;
use gpui_component::select::{Select, SelectEvent, SelectState};
use gpui_component::{ActiveTheme as _, IndexPath, Root, v_flex};
use medatat_core::{FieldIdx, WidgetKind, WidgetSpec, format_date, parse_date};
use std::cell::RefCell;
use std::rc::Rc;

/// One option of a radio group or select, in the shape `gpui-component`'s searchable list
/// wants. The *value* is the option code, never the label — the code is what
/// `medatat_core::Value::Opt` stores, so a label change can never silently retype data.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OptionItem {
    code: SharedString,
    label: SharedString,
}

impl SearchableListItem for OptionItem {
    type Value = SharedString;

    fn title(&self) -> SharedString {
        self.label.clone()
    }

    fn value(&self) -> &Self::Value {
        &self.code
    }
}

/// A change originating in a widget, expressed without any `gpui-component` type.
pub enum WidgetChange {
    /// Raw text exactly as it stands in the input. Not parsed — parsing is the form
    /// layer's job, because a half-typed value must stay visible.
    Text(String),
    /// An option code was picked, or the selection was cleared.
    Option(Option<String>),
    /// The field took focus. The form layer needs this to know which field an inbound
    /// sync edit must not overwrite.
    Focused,
    /// The user left the field — blur, Tab, or Enter. This is the moment a value that is
    /// only canonicalised on exit (time, R8) becomes final, and the moment to persist.
    Committed,
}

/// A callback the form layer supplies so a click inside a widget can reach it without the
/// widget module knowing what a `FormView` is.
pub type OnPick = Rc<dyn Fn(FieldIdx, Option<String>, &mut Window, &mut App)>;

/// Initialises `gpui-component`. Must run once, at the application entry point, before
/// any widget in this module is constructed.
pub fn init(cx: &mut App) {
    gpui_component::init(cx);
}

/// Opens a window hosting `build`'s view inside `gpui-component`'s `Root`.
///
/// `Root` is not decoration: it is where popovers, the select menu, and the date calendar
/// are rendered. Wrapping the bootstrap here rather than in `main.rs` keeps the rule that
/// no `gpui-component` type is named outside this module.
pub fn open_root_window<V: Render>(
    options: WindowOptions,
    build: impl FnOnce(&mut Window, &mut App) -> Entity<V>,
    cx: &mut App,
) -> anyhow::Result<()> {
    cx.open_window(options, |window, cx| {
        let view: AnyView = build(window, cx).into();
        cx.new(|cx| Root::new(view, window, cx))
    })?;
    Ok(())
}

/// Per-field editing state. Radio needs no entity at all, which materially cuts the count
/// on a large form.
pub enum WidgetState {
    Input(Entity<InputState>),
    Textarea(Entity<TextareaState>),
    Date(Entity<DatePickerState>),
    Select(Entity<SelectState<Vec<OptionItem>>>),
    /// Radio keeps no *value* state — the selection lives in the `FormInstance` — but it
    /// still needs a focus handle, or Tab would skip straight over the field.
    Radio(FocusHandle),
}

impl WidgetState {
    /// Creates the state a spec needs. Called once per field when a form opens — eagerly,
    /// because lazy creation breaks Tab traversal, scroll-into-field, and Cmd-F, which is
    /// exactly the feel R15 exists to protect.
    pub fn for_spec(spec: &WidgetSpec, window: &mut Window, cx: &mut App) -> Self {
        match spec.kind {
            WidgetKind::Radio => WidgetState::Radio(cx.focus_handle()),

            WidgetKind::Textarea => {
                let v = spec.value.clone();
                let rows = spec.rows.max(1) as usize;
                WidgetState::Textarea(cx.new(|cx| {
                    let mut s = TextareaState::new(window, cx).rows(rows);
                    if !v.is_empty() {
                        s = s.default_value(v);
                    }
                    s
                }))
            }

            WidgetKind::Date => {
                let parsed = parse_date(&spec.value).ok();
                WidgetState::Date(cx.new(|cx| {
                    let mut s = DatePickerState::new(window, cx).date_format("%Y-%m-%d");
                    if let Some(d) = parsed {
                        s.set_date(d, window, cx);
                    }
                    s
                }))
            }

            WidgetKind::Select => {
                let items = options_of(spec);
                // `spec.value` is the option *code* — `Value::Opt` stores the code.
                let selected = index_of_code(spec, Some(spec.value.as_str()));
                let searchable = spec.searchable;
                WidgetState::Select(cx.new(|cx| {
                    SelectState::new(
                        items,
                        selected.map(|i| IndexPath::default().row(i)),
                        window,
                        cx,
                    )
                    .searchable(searchable)
                }))
            }

            WidgetKind::Text | WidgetKind::Numeric | WidgetKind::Time => {
                let v = spec.value.clone();
                let ph = placeholder_for(spec.kind);
                let is_time = spec.kind == WidgetKind::Time;
                WidgetState::Input(cx.new(|cx| {
                    let mut s = InputState::new(window, cx).placeholder(ph);
                    // R8 rejection half. `validate` reverts to the previous text and
                    // returns, so an illegal character never lands and the caret never
                    // moves — which is what rejecting a keystroke has to mean.
                    if is_time {
                        s = s.validate(|proposed, _| time_input::accepts(proposed));
                    }
                    if !v.is_empty() {
                        s = s.default_value(v);
                    }
                    s
                }))
            }
        }
    }

    pub fn as_input(&self) -> Option<&Entity<InputState>> {
        match self {
            WidgetState::Input(e) => Some(e),
            _ => None,
        }
    }

    /// Reads the current text out of whichever state backs this widget.
    pub fn text(&self, cx: &App) -> String {
        match self {
            WidgetState::Input(e) => e.read(cx).value().to_string(),
            WidgetState::Textarea(e) => e.read(cx).value().to_string(),
            WidgetState::Date(e) => e
                .read(cx)
                .date()
                .start()
                .map(format_date)
                .unwrap_or_default(),
            WidgetState::Select(e) => e
                .read(cx)
                .selected_value()
                .map(|v| v.to_string())
                .unwrap_or_default(),
            WidgetState::Radio(_) => String::new(),
        }
    }

    /// Moves a select's visible selection to the option at `i`.
    ///
    /// Only the widget's own display — the value still reaches the model through the form
    /// layer, so there is exactly one path by which a `Value::Opt` is written.
    pub fn show_selected(&self, i: usize, window: &mut Window, cx: &mut App) {
        if let WidgetState::Select(e) = self {
            e.update(cx, |s, cx| {
                s.set_selected_index(Some(IndexPath::default().row(i)), window, cx)
            });
        }
    }

    /// Where focus goes when the form sends it here. Every kind has one, so `focus_order`
    /// (R12-adjacent, but the reason a keyboard-only abstractor can work at all) never has
    /// a hole in it.
    pub fn focus_handle(&self, cx: &App) -> FocusHandle {
        match self {
            WidgetState::Input(e) => e.focus_handle(cx),
            WidgetState::Textarea(e) => e.focus_handle(cx),
            WidgetState::Date(e) => e.focus_handle(cx),
            WidgetState::Select(e) => e.focus_handle(cx),
            WidgetState::Radio(h) => h.clone(),
        }
    }
}

/// Subscribes to a widget's own events, translating them into a [`WidgetChange`].
///
/// `subscribe_in` rather than `subscribe`, because committing a time field has to write
/// canonical text back into the input, and that needs a `Window`.
///
/// Returns `None` for a stateless widget (radio), which reports through [`OnPick`] instead.
pub fn subscribe<V: 'static>(
    state: &WidgetState,
    kind: WidgetKind,
    window: &Window,
    cx: &mut Context<V>,
    on_change: impl Fn(&mut V, WidgetChange, &mut Window, &mut Context<V>) + 'static,
) -> Option<Subscription> {
    match state {
        // The two text arms are duplicated rather than shared: `InputState` and
        // `TextareaState` are different instantiations of a generic whose name
        // `gpui-component` does not export, so there is no type to write a helper over.
        WidgetState::Input(e) => {
            // The text as of the last event, so the separator rule can tell a keystroke
            // that grew the field from a deletion that shrank it back through three digits.
            let previous = RefCell::new(e.read(cx).value().to_string());
            Some(
                cx.subscribe_in(e, window, move |this, ent, ev: &InputEvent, window, cx| {
                    let change = match ev {
                        InputEvent::Change => {
                            let mut text = ent.read(cx).value().to_string();
                            if kind == WidgetKind::Time {
                                text = apply_time_separator(ent, &previous, text, window, cx);
                            }
                            previous.replace(text.clone());
                            WidgetChange::Text(text)
                        }
                        InputEvent::Focus => WidgetChange::Focused,
                        InputEvent::Blur | InputEvent::PressEnter { .. } => WidgetChange::Committed,
                    };
                    on_change(this, change, window, cx);
                }),
            )
        }
        WidgetState::Textarea(e) => {
            Some(
                cx.subscribe_in(e, window, move |this, ent, ev: &InputEvent, window, cx| {
                    let change = match ev {
                        InputEvent::Change => WidgetChange::Text(ent.read(cx).value().to_string()),
                        InputEvent::Focus => WidgetChange::Focused,
                        // Enter inserts a newline in a textarea; only blur ends the edit.
                        InputEvent::Blur => WidgetChange::Committed,
                        InputEvent::PressEnter { .. } => return,
                    };
                    on_change(this, change, window, cx);
                }),
            )
        }
        WidgetState::Date(e) => Some(cx.subscribe_in(
            e,
            window,
            move |this, _, ev: &DatePickerEvent, window, cx| {
                let DatePickerEvent::Change(date) = ev;
                let text = date.start().map(format_date).unwrap_or_default();
                on_change(this, WidgetChange::Text(text), window, cx);
                on_change(this, WidgetChange::Committed, window, cx);
            },
        )),
        WidgetState::Select(e) => Some(cx.subscribe_in(
            e,
            window,
            move |this, _, ev: &SelectEvent<Vec<OptionItem>>, window, cx| {
                let SelectEvent::Confirm(code) = ev;
                let code = code.as_ref().map(|c| c.to_string());
                on_change(this, WidgetChange::Option(code), window, cx);
            },
        )),
        WidgetState::Radio(_) => None,
    }
}

/// The `Cmd/Ctrl-F` field palette's query box.
///
/// A newtype rather than a bare `Entity<InputState>` so the form layer can hold, focus, and
/// subscribe to it without naming a `gpui-component` type.
pub struct PaletteInput(Entity<InputState>);

impl PaletteInput {
    pub fn new(window: &mut Window, cx: &mut App) -> Self {
        PaletteInput(cx.new(|cx| InputState::new(window, cx).placeholder("Find field…")))
    }

    pub fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.0.focus_handle(cx)
    }

    /// Fires on every keystroke in the query box with the current text.
    pub fn subscribe<V: 'static>(
        &self,
        window: &Window,
        cx: &mut Context<V>,
        on_query: impl Fn(&mut V, String, &mut Window, &mut Context<V>) + 'static,
    ) -> Subscription {
        cx.subscribe_in(
            &self.0,
            window,
            move |this, ent, ev: &InputEvent, window, cx| {
                if matches!(ev, InputEvent::Change) {
                    on_query(this, ent.read(cx).value().to_string(), window, cx);
                }
            },
        )
    }
}

/// The theme's focus/selection ring. Exposed so views above this module can draw selection
/// chrome without reaching for `gpui-component`'s theme trait themselves.
pub fn ring_color(cx: &App) -> gpui::Hsla {
    cx.theme().ring
}

/// A bare query box, for the worklist filter. Same input type as the palette so the two
/// search surfaces in the app cannot drift apart.
pub fn render_filter(input: &PaletteInput) -> AnyElement {
    Input::new(&input.0).into_any_element()
}

/// The field palette: a query box over `medatat_core::view::search_fields` results.
///
/// Rendered as an overlay so it never displaces the form underneath — a palette that
/// reflows 300 fields on open would be worse than no palette.
pub fn render_palette(
    palette: &PaletteInput,
    matches: &[(FieldIdx, String)],
    on_choose: &OnChoose,
) -> AnyElement {
    let rows = matches.iter().take(8).map(|(idx, label)| {
        let choose = on_choose.clone();
        let idx = *idx;
        div()
            .id(SharedString::from(format!("palette-{}", idx.0)))
            .px_2()
            .py_1()
            .rounded_sm()
            .cursor_pointer()
            .child(SharedString::from(label.clone()))
            .on_click(move |_, window, cx| choose(idx, window, cx))
    });

    div()
        .absolute()
        .top_8()
        .right_8()
        .w(px(360.))
        .p_2()
        .gap_1()
        .rounded_md()
        .border_1()
        .child(Input::new(&palette.0))
        .child(v_flex().gap_1().children(rows))
        .into_any_element()
}

/// What the palette calls when a field is chosen.
pub type OnChoose = Rc<dyn Fn(FieldIdx, &mut Window, &mut App)>;

/// What a click on a field in design mode calls. Same shape as [`OnChoose`], different
/// meaning: this one selects for the inspector rather than moving focus.
pub type OnSelectField = Rc<dyn Fn(FieldIdx, &mut Window, &mut App)>;

/// R8 separator half: insert the `:` after the third digit, and only there.
///
/// Runs on change rather than on key-down because `validate` cannot rewrite text. Delegates
/// the decision to `time_input::filter_input` so the rule has exactly one definition, and
/// acts only on `AcceptWithColon` — `Reject` is already unreachable here, having been
/// stopped by the `validate` hook before the text ever landed.
fn apply_time_separator<V: 'static>(
    ent: &Entity<InputState>,
    previous: &RefCell<String>,
    text: String,
    window: &mut Window,
    cx: &mut Context<V>,
) -> String {
    let state = ent.read(cx);
    let caret_at_end = state.cursor() >= state.value().len();
    match time_input::filter_input(&previous.borrow(), &text, caret_at_end) {
        time_input::TimeEdit::AcceptWithColon(with_colon) => {
            // `set_value` suppresses events while it writes, so this cannot recurse.
            ent.update(cx, |s, cx| s.set_value(with_colon.clone(), window, cx));
            with_colon
        }
        _ => text,
    }
}

fn options_of(spec: &WidgetSpec) -> Vec<OptionItem> {
    spec.options
        .iter()
        .map(|(code, label)| OptionItem {
            code: SharedString::from(code.clone()),
            label: SharedString::from(label.clone()),
        })
        .collect()
}

fn index_of_code(spec: &WidgetSpec, code: Option<&str>) -> Option<usize> {
    let code = code.filter(|c| !c.is_empty())?;
    spec.options.iter().position(|(c, _)| c == code)
}

fn placeholder_for(kind: WidgetKind) -> &'static str {
    match kind {
        // 24-hour, always. The placeholder is the only affordance telling the user so.
        WidgetKind::Time => "HH:MM",
        WidgetKind::Numeric => "0",
        WidgetKind::Date => "YYYY-MM-DD",
        _ => "",
    }
}

/// Renders one field as a form `Field`.
pub fn render_field(
    spec: &WidgetSpec,
    state: &WidgetState,
    selected: Option<&str>,
    on_pick: &OnPick,
    mode: RenderMode,
    on_select: &OnSelectField,
    cx: &App,
) -> Field {
    let mut f = field()
        .label(SharedString::from(spec.label.clone()))
        .required(spec.required)
        .col_span(spec.col_span as u16);

    if let Some(err) = &spec.error {
        f = f.description(SharedString::from(err.to_string()));
    }

    let widget = match (spec.kind, state) {
        (WidgetKind::Numeric, WidgetState::Input(e)) => NumberInput::new(e).into_any_element(),
        (WidgetKind::Time, WidgetState::Input(e)) => render_time(e),
        (_, WidgetState::Input(e)) => Input::new(e).into_any_element(),
        (_, WidgetState::Textarea(e)) => Textarea::new(e).into_any_element(),
        (_, WidgetState::Date(e)) => DatePicker::new(e)
            .placeholder("YYYY-MM-DD")
            .cleanable(true)
            .into_any_element(),
        (_, WidgetState::Select(e)) => Select::new(e)
            .placeholder(SharedString::from(format!("Select {}", spec.label)))
            .cleanable(true)
            .into_any_element(),
        (_, WidgetState::Radio(h)) => render_radio(spec, selected, on_pick, h, cx),
    };

    match mode {
        RenderMode::Runtime => f.child(widget),
        RenderMode::Design { .. } => f.child(design_chrome(widget, spec.idx, mode, on_select, cx)),
    }
}

/// Design-mode chrome around an otherwise identical widget.
///
/// One overlay does both jobs the spec asks of design mode: it swallows every mouse event,
/// which is what "widgets render but are non-interactive" means in practice, and it turns a
/// click into a selection rather than a focus. The widget underneath is byte-for-byte the
/// element the runtime renders.
fn design_chrome(
    widget: AnyElement,
    idx: FieldIdx,
    mode: RenderMode,
    on_select: &OnSelectField,
    cx: &App,
) -> AnyElement {
    let select = on_select.clone();
    let border = if mode.selects_field(idx) {
        cx.theme().ring
    } else {
        gpui::transparent_black()
    };

    div()
        .relative()
        .rounded_sm()
        .border_2()
        .border_color(border)
        .child(widget)
        .child(
            div()
                .id(SharedString::from(format!("design-{}", idx.0)))
                .absolute()
                .top_0()
                .left_0()
                .size_full()
                .occlude()
                .cursor_pointer()
                .on_click(move |_, window, cx| select(idx, window, cx)),
        )
        .into_any_element()
}

/// The 24-hour time input (R8): a plain input plus Up/Down stepping.
///
/// The capture phase is load-bearing. `InputState` binds `up`/`down` to `MoveUp`/`MoveDown`
/// in its own key context, so a bubble-phase handler would never see them — capture runs
/// first, and `stop_propagation` keeps the caret from moving instead.
fn render_time(state: &Entity<InputState>) -> AnyElement {
    let stepper = state.clone();
    div()
        .capture_key_down(move |ev, window, cx| {
            let step = match ev.keystroke.key.as_str() {
                "up" => 1,
                "down" => -1,
                _ => return,
            };
            // Shift moves the hour, matching the convention every other stepper uses.
            let minutes = if ev.keystroke.modifiers.shift {
                step * 60
            } else {
                step
            };
            let nudged = time_input::nudge(&stepper.read(cx).value(), minutes);
            stepper.update(cx, |s, cx| s.set_value(nudged, window, cx));
            cx.stop_propagation();
        })
        .child(Input::new(state))
        .into_any_element()
}

/// A radio group (R9). Holds no value state — the selection lives in the `FormInstance`, so
/// there is no second copy to keep in sync — only a focus handle.
fn render_radio(
    spec: &WidgetSpec,
    selected: Option<&str>,
    on_pick: &OnPick,
    focus: &FocusHandle,
    cx: &App,
) -> AnyElement {
    let idx = spec.idx;
    let codes: Vec<String> = spec.options.iter().map(|(c, _)| c.clone()).collect();
    let pick = on_pick.clone();

    let group = RadioGroup::horizontal(SharedString::from(format!("radio-{}", idx.0)))
        .selected_index(index_of_code(spec, selected))
        .children(spec.options.iter().map(|(code, label)| {
            Radio::new(SharedString::from(format!("radio-{}-{}", idx.0, code)))
                .label(SharedString::from(label.clone()))
        }))
        .on_click(move |ix, window, cx| {
            pick(idx, codes.get(*ix).cloned(), window, cx);
        });

    // `RadioGroup` is not a `Div`, so the focus handle goes on a wrapper. Without this the
    // group is mouse-only and Tab steps over the field entirely. The ring goes on the same
    // wrapper for the same reason: focus lives here rather than on an individual radio, so
    // this is the only element that can show it — and a keyboard user must never guess.
    let ring = cx.theme().ring;
    div()
        .track_focus(focus)
        .rounded_sm()
        .border_2()
        .border_color(gpui::transparent_black())
        .focus(|s| s.border_color(ring))
        .child(group)
        .into_any_element()
}

/// The 1–3 column section grid (R12).
///
/// Kept behind this function so that if `gpui-component`'s `Form` ever mishandles spans or
/// gaps, the fallback — a hand-rolled `flex_wrap` grid with fractional widths — is a
/// one-file swap rather than a rewrite.
pub fn form_grid(columns: u8, fields: Vec<Field>) -> impl IntoElement {
    v_form()
        .columns(columns.clamp(1, 3) as usize)
        .label_width(px(160.))
        .children(fields)
}
