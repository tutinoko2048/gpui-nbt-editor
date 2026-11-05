//! The "Add tag" dialog body: name, type and value for a new child tag.

use gpui_kit::component::{
    ActiveTheme, IndexPath, StyledExt, WindowExt, h_flex, v_flex,
    input::{Input, InputEvent, InputState},
    select::{Select, SelectState},
};
use gpui_kit::prelude::FluentBuilder;
use gpui_kit::*;

use crate::{NbtEditor, nbt, type_badge};

/// Where a new tag goes.
#[derive(Clone)]
pub struct AddTarget {
    pub parent_id: SharedString,
    pub is_compound: bool,
    /// A non-empty list fixes the element type.
    pub fixed_kind: Option<&'static str>,
}

pub struct AddTagDialog {
    editor: WeakEntity<NbtEditor>,
    target: AddTarget,
    name: Entity<InputState>,
    kind: Entity<SelectState<Vec<&'static str>>>,
    value: Entity<InputState>,
    error: Option<SharedString>,
    _subscriptions: Vec<Subscription>,
}

impl AddTagDialog {
    pub fn new(
        editor: WeakEntity<NbtEditor>,
        target: AddTarget,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let name = cx.new(|cx| InputState::new(window, cx).placeholder("Name"));
        let value = cx.new(|cx| InputState::new(window, cx).placeholder("Value"));
        let kind = cx.new(|cx| {
            SelectState::new(nbt::NEW_TYPES.to_vec(), Some(IndexPath::new(2)), window, cx)
        });

        let submit_on_enter = |this: &mut Self, _: &Entity<InputState>, event: &InputEvent, window: &mut Window, cx: &mut Context<Self>| {
            if matches!(event, InputEvent::PressEnter { .. }) {
                this.submit(window, cx);
            }
        };
        let subscriptions = vec![
            cx.observe(&kind, |_, _, cx| cx.notify()),
            cx.subscribe_in(&name, window, submit_on_enter),
            cx.subscribe_in(&value, window, submit_on_enter),
        ];

        let first = if target.is_compound { &name } else { &value };
        first.update(cx, |input, cx| input.focus(window, cx));

        Self { editor, target, name, kind, value, error: None, _subscriptions: subscriptions }
    }

    fn kind(&self, cx: &App) -> &'static str {
        self.target
            .fixed_kind
            .or_else(|| self.kind.read(cx).selected_value().copied())
            .unwrap_or("Int")
    }

    pub fn submit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let kind = self.kind(cx);
        let name = self.name.read(cx).value().trim().to_string();
        let text = self.value.read(cx).value();
        let parent_id = self.target.parent_id.clone();

        let result = self.editor.update(cx, |editor, cx| {
            editor.insert_tag(&parent_id, name, kind, &text, window, cx)
        });
        match result {
            Ok(Ok(())) => {
                window.close_dialog(cx);
                self.editor.update(cx, |editor, cx| editor.focus(window, cx)).ok();
            }
            Ok(Err(err)) => {
                self.error = Some(err.into());
                cx.notify();
            }
            Err(_) => window.close_dialog(cx),
        }
    }
}

impl Render for AddTagDialog {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let kind = self.kind(cx);
        let takes_value = !matches!(kind, "List" | "Compound");
        let theme = cx.theme();
        let field = |label: &'static str, input: AnyElement| {
            v_flex()
                .gap_1()
                .child(div().text_xs().font_medium().text_color(theme.muted_foreground).child(label))
                .child(input)
        };

        v_flex()
            .gap_3()
            .child(
                div()
                    .text_sm()
                    .text_color(theme.muted_foreground)
                    .child(format!("Into {}", self.target.parent_id)),
            )
            .when(self.target.is_compound, |this| {
                this.child(field("Name", Input::new(&self.name).into_any_element()))
            })
            .child(field(
                "Type",
                match self.target.fixed_kind {
                    Some(kind) => h_flex()
                        .gap_2()
                        .child(type_badge(kind))
                        .child(
                            div()
                                .text_sm()
                                .text_color(theme.muted_foreground)
                                .child(format!("{kind} (list element type)")),
                        )
                        .into_any_element(),
                    None => Select::new(&self.kind).into_any_element(),
                },
            ))
            .when(takes_value, |this| {
                this.child(field("Value", Input::new(&self.value).into_any_element()))
            })
            .when_some(self.error.clone(), |this, err| {
                this.child(div().text_sm().text_color(theme.danger).child(err))
            })
    }
}
