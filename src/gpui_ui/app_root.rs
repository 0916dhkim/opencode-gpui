//! Window services owned by this application, using published toolkit APIs.
use gpui_kit::base::input::{Copy, Cut, InputContextMenuCapabilities, Paste, SelectAll};
use gpui_kit::component::{menu::PopupMenu, native_menu::NativeMenu};
use gpui_kit::prelude::FluentBuilder;
use gpui_kit::*;

use super::{Client, UI_FONT_FAMILY};

actions!(opencode_root, [NextFocus, PreviousFocus]);

pub(super) fn init(cx: &mut App) {
    cx.bind_keys([
        KeyBinding::new("tab", NextFocus, Some("OpenCodeRoot")),
        KeyBinding::new("shift-tab", PreviousFocus, Some("OpenCodeRoot")),
        KeyBinding::new(
            if cfg!(target_os = "macos") {
                "cmd-c"
            } else {
                "ctrl-c"
            },
            Copy,
            Some("OpenCodeRoot"),
        ),
    ]);
}

pub(super) struct AppRoot {
    client: Entity<Client>,
    popup: Option<(Entity<PopupMenu>, Point<Pixels>)>,
    popup_subscription: Option<Subscription>,
    _touch_subscription: Subscription,
}

impl AppRoot {
    pub(super) fn new(client: Entity<Client>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        #[cfg(target_os = "macos")]
        gpui_kit::base::install_window_hit_test_forwarder(window);
        let owner = cx.weak_entity();
        let touch_subscription =
            gpui_kit::base::TextSelection::observe_touch_selection(window, cx, move |cx| {
                let _ = owner.update(cx, |_, cx| cx.notify());
            });
        Self {
            client,
            popup: None,
            popup_subscription: None,
            _touch_subscription: touch_subscription,
        }
    }

    fn show_editor_menu(
        &mut self,
        focus: FocusHandle,
        capabilities: InputContextMenuCapabilities,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let policy = editor_menu_policy(capabilities);
        let popup = PopupMenu::build(window, cx, |menu, _, _| {
            menu.action_context(focus)
                .menu_with_disabled("Cut", Box::new(Cut), !policy.cut)
                .menu_with_disabled("Copy", Box::new(Copy), !policy.copy)
                .menu_with_disabled("Paste", Box::new(Paste), !policy.paste)
                .separator()
                .menu("Select All", Box::new(SelectAll))
        });
        self.popup_subscription = Some(cx.subscribe(&popup, |this, menu, _: &DismissEvent, cx| {
            if this
                .popup
                .as_ref()
                .is_some_and(|(current, _)| *current == menu)
            {
                this.popup = None;
                this.popup_subscription = None;
                cx.notify();
            }
        }));
        // Focus synchronously: the native editor associates this menu's focus
        // with its selection after the public context-menu callback returns.
        popup.focus_handle(cx).focus(window, cx);
        self.popup = Some((popup, window.mouse_position()));
        cx.notify();
    }

    fn touch_menu(&self, window: &mut Window, cx: &mut Context<Self>) -> Option<AnyElement> {
        use gpui_kit::base::{Button, TextSelection};
        let selection = TextSelection::touch_selection(window, cx)?;
        if !selection.is_menu_open() || selection.dragging().is_some() {
            return None;
        }
        let bounds = selection.bounds()?;
        let y = if bounds.top() >= px(48.) {
            bounds.top() - px(44.)
        } else {
            bounds.bottom() + px(12.)
        };
        let dark = self.client.read(cx).dark;
        let menu = div()
            .id("touch-selection-menu")
            .relative()
            .flex()
            .gap(px(4.))
            .p(px(4.))
            .rounded(px(7.))
            .border_1()
            .border_color(rgb(if dark { 0x30353a } else { 0xc8c3ba }))
            .bg(rgb(if dark { 0x191c1f } else { 0xfffdfa }))
            .text_color(rgb(if dark { 0xe8e5df } else { 0x252829 }))
            .text_size(px(13.))
            .shadow_lg()
            .child(
                Button::new("touch-copy")
                    .px(px(10.))
                    .h(px(36.))
                    .child("Copy")
                    .on_click(|_, window, cx| {
                        let text = TextSelection::selected_text(window, cx);
                        if !text.trim().is_empty() {
                            cx.write_to_clipboard(ClipboardItem::new_string(
                                text.trim().to_owned(),
                            ));
                        }
                        TextSelection::close_edit_menu(window, cx);
                    }),
            )
            .child(
                Button::new("touch-select-all")
                    .px(px(10.))
                    .h(px(36.))
                    .child("Select All")
                    .on_click(|_, window, cx| TextSelection::select_all(window, cx)),
            )
            .child(
                canvas(
                    |_, _, _| (),
                    |bounds, _, window, cx| {
                        TextSelection::register_touch_ui(bounds, window, cx);
                    },
                )
                .absolute()
                .inset_0(),
            );
        Some(
            deferred(
                anchored()
                    .position(point(bounds.left(), y))
                    .snap_to_window_with_margin(px(8.))
                    .child(menu),
            )
            .with_priority(gpui_kit::base::POPUP_PRIORITY)
            .into_any_element(),
        )
    }
}

impl Render for AppRoot {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        gpui_kit::base::TextSelection::activate_scope(Default::default(), window, cx);
        div()
            .id("opencode-root")
            .key_context("OpenCodeRoot")
            .on_action(|_: &NextFocus, window, cx| advance_focus(window, cx, Window::focus_next))
            .on_action(|_: &PreviousFocus, window, cx| {
                advance_focus(window, cx, Window::focus_prev)
            })
            .on_action(|_: &Copy, window, cx| {
                let text = gpui_kit::base::TextSelection::selected_text(window, cx);
                let text = text.trim();
                if text.is_empty() {
                    cx.propagate();
                } else {
                    cx.write_to_clipboard(ClipboardItem::new_string(text.to_owned()));
                }
            })
            .relative()
            .size_full()
            .font_family(UI_FONT_FAMILY)
            .child(gpui_kit::base::TextSelectionLayer)
            .child(self.client.clone())
            .children(self.touch_menu(window, cx))
            .when_some(self.popup.clone(), |view, (menu, position)| {
                view.child(
                    deferred(
                        anchored().child(
                            div()
                                .w(window.viewport_size().width)
                                .h(window.viewport_size().height)
                                .on_scroll_wheel(|_, _, cx| cx.stop_propagation())
                                .child(
                                    anchored()
                                        .position(position)
                                        .snap_to_window_with_margin(px(8.))
                                        .child(menu),
                                ),
                        ),
                    )
                    .with_priority(gpui_kit::base::POPUP_PRIORITY),
                )
            })
    }
}

fn advance_focus(window: &mut Window, cx: &mut App, advance: fn(&mut Window, &mut App)) {
    let Some(container) = gpui_kit::base::active_focus_trap(window, cx) else {
        advance(window, cx);
        return;
    };
    advance(window, cx);
    let first = window.focused(cx);
    while !container.contains_focused(window, cx) {
        advance(window, cx);
        if window.focused(cx) == first {
            container.focus(window, cx);
            break;
        }
    }
}

/// Install on every Component input, whose default Linux menu otherwise needs
/// Component's private Root plugin. Weak state avoids an editor callback cycle.
pub(super) fn editor_context_menu<S: Focusable + 'static>(
    state: &Entity<S>,
    capabilities: fn(&S) -> InputContextMenuCapabilities,
) -> impl Fn(NativeMenu, &mut Window, &mut App) -> NativeMenu + 'static {
    let state = state.downgrade();
    move |menu, window, cx| {
        if let Some(state) = state.upgrade() {
            let (focus, capabilities) = {
                let state = state.read(cx);
                (state.focus_handle(cx), capabilities(state))
            };
            if let Some(root) = window.root::<AppRoot>().flatten() {
                root.update(cx, |root, cx| {
                    root.show_editor_menu(focus, capabilities, window, cx)
                });
            }
        }
        // Empty NativeMenu deliberately does nothing; our retained PopupMenu
        // supplies Linux presentation without an upstream source patch.
        menu
    }
}

#[derive(Debug, PartialEq, Eq)]
struct EditorMenuPolicy {
    cut: bool,
    copy: bool,
    paste: bool,
}

fn editor_menu_policy(capabilities: InputContextMenuCapabilities) -> EditorMenuPolicy {
    EditorMenuPolicy {
        cut: capabilities.is_editable() && capabilities.is_copyable(),
        copy: capabilities.is_copyable(),
        paste: capabilities.is_editable(),
    }
}

#[cfg(test)]
mod tests {
    use super::{EditorMenuPolicy, InputContextMenuCapabilities, editor_menu_policy};

    #[test]
    fn editor_menu_respects_selection_masking_and_editability() {
        let selected = InputContextMenuCapabilities::new().selection(true);
        assert_eq!(
            editor_menu_policy(selected),
            EditorMenuPolicy {
                cut: true,
                copy: true,
                paste: true
            }
        );
        assert_eq!(
            editor_menu_policy(selected.readonly(true)),
            EditorMenuPolicy {
                cut: false,
                copy: true,
                paste: false
            }
        );
        assert_eq!(
            editor_menu_policy(selected.masked(true)),
            EditorMenuPolicy {
                cut: false,
                copy: false,
                paste: true
            }
        );
        assert_eq!(
            editor_menu_policy(selected.disabled(true)),
            EditorMenuPolicy {
                cut: false,
                copy: true,
                paste: false
            }
        );
        assert_eq!(
            editor_menu_policy(InputContextMenuCapabilities::new()),
            EditorMenuPolicy {
                cut: false,
                copy: false,
                paste: true
            }
        );
    }
}
