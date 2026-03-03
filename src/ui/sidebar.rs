use crate::state::AppState;
use chrono::{DateTime, Utc};
use gpui::prelude::*;
use gpui::*;
use gpui_component::input::{Input, InputEvent, InputState};
use std::collections::HashSet;

pub struct SidebarView {
    app_state: Entity<AppState>,
    collapsed_workspaces: HashSet<uuid::Uuid>,
    thread_context_menu: Option<uuid::Uuid>,
    renaming_thread_id: Option<uuid::Uuid>,
    rename_input: Option<Entity<InputState>>,
}

#[derive(Clone)]
enum SidebarDragItem {
    Workspace(uuid::Uuid),
    Thread {
        workspace_id: uuid::Uuid,
        thread_id: uuid::Uuid,
    },
}

struct DragPreview {
    label: SharedString,
}

impl DragPreview {
    fn new(label: impl Into<SharedString>) -> Self {
        Self {
            label: label.into(),
        }
    }
}

impl Render for DragPreview {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .px_2()
            .py_1()
            .rounded_md()
            .bg(rgb(0x0e639c))
            .text_color(white())
            .child(self.label.clone())
    }
}

impl SidebarView {
    pub fn new(app_state: Entity<AppState>, cx: &mut Context<Self>) -> Self {
        cx.observe(&app_state, |_, _, cx| cx.notify()).detach();
        cx.observe_keystrokes(|this, event, _window, cx| {
            if this.renaming_thread_id.is_some() && event.keystroke.key == "escape" {
                this.cancel_rename(cx);
            }
        })
        .detach();

        Self {
            app_state,
            collapsed_workspaces: HashSet::new(),
            thread_context_menu: None,
            renaming_thread_id: None,
            rename_input: None,
        }
    }

    fn begin_rename(
        &mut self,
        thread_id: uuid::Uuid,
        current_name: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let input = cx.new(|cx| {
            InputState::new(window, cx)
                .default_value(current_name)
                .placeholder("Thread name")
        });
        cx.subscribe(&input, |this, _, event: &InputEvent, cx| {
            if matches!(event, InputEvent::PressEnter { .. } | InputEvent::Blur) {
                this.commit_rename(cx);
            }
        })
        .detach();
        input.update(cx, |state, cx| {
            state.focus(window, cx);
        });
        self.renaming_thread_id = Some(thread_id);
        self.rename_input = Some(input);
        self.thread_context_menu = None;
        cx.notify();
    }

    fn commit_rename(&mut self, cx: &mut Context<Self>) {
        let Some(thread_id) = self.renaming_thread_id else {
            return;
        };
        let Some(input) = &self.rename_input else {
            return;
        };
        let name = input.read(cx).value().to_string();
        self.app_state.update(cx, |state, cx| {
            let _ = state.rename_thread(cx, thread_id, name);
        });
        self.renaming_thread_id = None;
        self.rename_input = None;
        cx.notify();
    }

    fn cancel_rename(&mut self, cx: &mut Context<Self>) {
        self.renaming_thread_id = None;
        self.rename_input = None;
        cx.notify();
    }
}

impl Render for SidebarView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let (workspaces, active_thread_id) = {
            let state = self.app_state.read(cx);
            (state.workspaces.clone(), state.active_thread_id)
        };

        div()
            .id("sidebar-root")
            .flex()
            .flex_col()
            .w(px(260.0))
            .h_full()
            .overflow_y_scroll()
            .bg(rgb(0x252526))
            .border_r_1()
            .border_color(rgb(0x3c3c3c))
            .p_3()
            .gap_2()
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, _, cx| {
                    this.thread_context_menu = None;
                    if this.renaming_thread_id.is_none() {
                        cx.notify();
                    }
                }),
            )
            .child(
                div()
                    .id("new-workspace-button")
                    .bg(rgb(0x0e639c))
                    .text_color(white())
                    .p_2()
                    .rounded_md()
                    .cursor_pointer()
                    .child("New Workspace")
                    .on_click(cx.listener(|this, _, _, cx| {
                        let picker = cx.prompt_for_paths(PathPromptOptions {
                            files: false,
                            directories: true,
                            multiple: false,
                            prompt: Some("Select workspace folder".into()),
                        });
                        let app_state = this.app_state.clone();
                        cx.spawn(
                            move |_sidebar: gpui::WeakEntity<SidebarView>,
                                  cx: &mut gpui::AsyncApp| {
                                let mut cx = cx.clone();
                                async move {
                                    let path = match picker.await {
                                        Ok(Ok(Some(paths))) => paths.into_iter().next(),
                                        _ => None,
                                    };
                                    if let Some(path) = path {
                                        let _ = app_state.update(
                                            &mut cx,
                                            |state: &mut AppState, cx| {
                                                let ws_id = state.add_workspace_from_path(cx, path);
                                                let _ = state.add_thread(cx, ws_id, "Thread 1");
                                            },
                                        );
                                    }
                                }
                            },
                        )
                        .detach();
                    })),
            )
            .children(
                workspaces
                    .into_iter()
                    .enumerate()
                    .map(|(ws_index, workspace)| {
                        let ws_id = workspace.id;
                        let workspace_name = workspace.name.clone();
                        let is_collapsed = self.collapsed_workspaces.contains(&ws_id);

                        let header = div()
                            .id(("workspace-header", ws_index))
                            .mt_2()
                            .p_2()
                            .rounded_md()
                            .bg(rgb(0x2d2d30))
                            .text_color(rgb(0xcccccc))
                            .cursor_pointer()
                            .child(if is_collapsed {
                                format!("▶ {}", workspace.name)
                            } else {
                                format!("▼ {}", workspace.name)
                            })
                            .on_click(cx.listener(move |this, _, _, cx| {
                                if this.collapsed_workspaces.contains(&ws_id) {
                                    this.collapsed_workspaces.remove(&ws_id);
                                } else {
                                    this.collapsed_workspaces.insert(ws_id);
                                }
                                cx.notify();
                            }))
                            .on_drag(
                                SidebarDragItem::Workspace(ws_id),
                                move |_dragged, _, _, cx| {
                                    cx.new(|_| {
                                        DragPreview::new(format!("Workspace: {workspace_name}"))
                                    })
                                },
                            )
                            .on_drop(cx.listener(move |this, dragged: &SidebarDragItem, _, cx| {
                                if let SidebarDragItem::Workspace(dragged_ws_id) = dragged {
                                    this.app_state.update(cx, |state, cx| {
                                        state.reorder_workspaces(cx, *dragged_ws_id, ws_id);
                                    });
                                }
                            }))
                            .drag_over::<SidebarDragItem>(|style, _, _, _| style.bg(rgb(0x3a3a40)));

                        if is_collapsed {
                            return div().child(header);
                        }

                        let new_thread_button = div()
                            .id(("workspace-new-thread", ws_index))
                            .text_color(rgb(0x8f8f8f))
                            .text_sm()
                            .cursor_pointer()
                            .pl_2()
                            .child("+ New Thread")
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.app_state.update(cx, |state, cx| {
                                    let thread_index = state
                                        .workspaces
                                        .iter()
                                        .find(|w| w.id == ws_id)
                                        .map(|w| w.threads.len() + 1)
                                        .unwrap_or(1);
                                    let name = format!("Thread {thread_index}");
                                    let _ = state.add_thread(cx, ws_id, &name);
                                });
                            }));

                        let thread_list = workspace.threads.into_iter().enumerate().map(
                            |(thread_index, thread)| {
                                let thread_id = thread.id;
                                let is_active = active_thread_id == Some(thread_id);
                                let has_unread_stop = !is_active
                                    && self.app_state.read(cx).thread_has_unread_stop(thread_id);
                                let thread_dom_id = ws_index * 1000 + thread_index;
                                let thread_name = thread.name.clone();
                                let is_renaming = self.renaming_thread_id == Some(thread_id);
                                let rename_input = self.rename_input.clone();
                                let trailing = if has_unread_stop {
                                    div()
                                        .w(px(8.0))
                                        .h(px(8.0))
                                        .rounded_full()
                                        .bg(rgb(0xff9d00))
                                        .into_any_element()
                                } else {
                                    div()
                                        .text_xs()
                                        .text_color(rgb(0x8f8f8f))
                                        .child(relative_time_short(thread.updated_at))
                                        .into_any_element()
                                };

                                let row = div()
                                    .id(("thread-item", thread_dom_id))
                                    .pl_4()
                                    .pr_2()
                                    .py_1()
                                    .rounded_sm()
                                    .bg(if is_active {
                                        rgb(0x37373d)
                                    } else {
                                        rgba(0x00000000)
                                    })
                                    .text_color(rgb(0xcccccc))
                                    .cursor_pointer()
                                    .on_mouse_down(
                                        MouseButton::Right,
                                        cx.listener(move |this, _, _, cx| {
                                            this.thread_context_menu = Some(thread_id);
                                            cx.notify();
                                        }),
                                    )
                                    .on_drag(
                                        SidebarDragItem::Thread {
                                            workspace_id: ws_id,
                                            thread_id,
                                        },
                                        move |_dragged, _, _, cx| {
                                            cx.new(|_| {
                                                DragPreview::new(format!("Thread: {thread_name}"))
                                            })
                                        },
                                    )
                                    .on_drop(cx.listener(
                                        move |this, dragged: &SidebarDragItem, _, cx| {
                                            if let SidebarDragItem::Thread {
                                                workspace_id,
                                                thread_id: dragged_thread_id,
                                            } = dragged
                                                && *workspace_id == ws_id
                                            {
                                                this.app_state.update(cx, |state, cx| {
                                                    state.reorder_threads(
                                                        cx,
                                                        *workspace_id,
                                                        *dragged_thread_id,
                                                        thread_id,
                                                    );
                                                });
                                            }
                                        },
                                    ))
                                    .drag_over::<SidebarDragItem>(move |style, dragged, _, _| {
                                        match dragged {
                                            SidebarDragItem::Thread { workspace_id, .. }
                                                if *workspace_id == ws_id =>
                                            {
                                                style.bg(rgb(0x42424a))
                                            }
                                            _ => style,
                                        }
                                    })
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        this.thread_context_menu = None;
                                        this.app_state.update(cx, |state, cx| {
                                            state.set_active_thread(cx, thread_id);
                                        });
                                    }))
                                    .child(
                                        div()
                                            .flex()
                                            .items_center()
                                            .justify_between()
                                            .gap_2()
                                            .child(if is_renaming {
                                                if let Some(input) = rename_input {
                                                    div()
                                                        .flex_1()
                                                        .child(Input::new(&input))
                                                        .into_any_element()
                                                } else {
                                                    div()
                                                        .flex_1()
                                                        .child(thread.name.clone())
                                                        .into_any_element()
                                                }
                                            } else {
                                                div()
                                                    .flex_1()
                                                    .child(thread.name.clone())
                                                    .into_any_element()
                                            })
                                            .child(trailing),
                                    );

                                let context_menu = if self.thread_context_menu == Some(thread_id)
                                    && !is_renaming
                                {
                                    div()
                                        .pl_6()
                                        .pt_1()
                                        .flex()
                                        .flex_col()
                                        .gap_1()
                                        .child(
                                            div()
                                                .id(("thread-menu-rename", thread_dom_id))
                                                .px_2()
                                                .py_1()
                                                .rounded_sm()
                                                .bg(rgb(0x2d2d30))
                                                .cursor_pointer()
                                                .child("Rename")
                                                .on_click(cx.listener(
                                                    move |this, _, window, cx| {
                                                        this.begin_rename(
                                                            thread_id,
                                                            thread.name.clone(),
                                                            window,
                                                            cx,
                                                        );
                                                    },
                                                )),
                                        )
                                        .child(
                                            div()
                                                .id(("thread-menu-delete", thread_dom_id))
                                                .px_2()
                                                .py_1()
                                                .rounded_sm()
                                                .bg(rgb(0x2d2d30))
                                                .cursor_pointer()
                                                .child("Delete thread")
                                                .on_click(cx.listener(move |this, _, _, cx| {
                                                    this.thread_context_menu = None;
                                                    this.app_state.update(cx, |state, cx| {
                                                        let _ = state.delete_thread(cx, thread_id);
                                                    });
                                                })),
                                        )
                                        .child(
                                            div()
                                                .id(("thread-menu-unread", thread_dom_id))
                                                .px_2()
                                                .py_1()
                                                .rounded_sm()
                                                .bg(rgb(0x2d2d30))
                                                .cursor_pointer()
                                                .child("Mark as unread")
                                                .on_click(cx.listener(move |this, _, _, cx| {
                                                    this.thread_context_menu = None;
                                                    this.app_state.update(cx, |state, cx| {
                                                        state.mark_thread_unread(cx, thread_id);
                                                    });
                                                })),
                                        )
                                        .into_any_element()
                                } else {
                                    div().into_any_element()
                                };

                                div().child(row).child(context_menu)
                            },
                        );

                        div()
                            .child(header)
                            .child(new_thread_button)
                            .children(thread_list)
                    }),
            )
    }
}

fn relative_time_short(timestamp: DateTime<Utc>) -> String {
    let delta = (Utc::now() - timestamp).num_seconds().max(0);
    if delta < 60 {
        return format!("{delta}s");
    }
    if delta < 3_600 {
        return format!("{}m", delta / 60);
    }
    if delta < 86_400 {
        return format!("{}h", delta / 3_600);
    }
    if delta < 604_800 {
        return format!("{}d", delta / 86_400);
    }
    format!("{}w", delta / 604_800)
}

#[cfg(test)]
mod tests {
    use super::relative_time_short;
    use chrono::{Duration, Utc};

    #[test]
    fn relative_time_short_formats_units() {
        assert_eq!(
            relative_time_short(Utc::now() - Duration::seconds(12)),
            "12s"
        );
        assert_eq!(relative_time_short(Utc::now() - Duration::minutes(5)), "5m");
        assert_eq!(relative_time_short(Utc::now() - Duration::hours(3)), "3h");
    }
}
