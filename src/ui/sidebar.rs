use crate::state::AppState;
use gpui::prelude::*;
use gpui::*;
use std::collections::HashSet;

pub struct SidebarView {
    app_state: Entity<AppState>,
    collapsed_workspaces: HashSet<uuid::Uuid>,
}

impl SidebarView {
    pub fn new(app_state: Entity<AppState>, cx: &mut Context<Self>) -> Self {
        cx.observe(&app_state, |_, _, cx| cx.notify()).detach();
        Self {
            app_state,
            collapsed_workspaces: HashSet::new(),
        }
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
                        this.app_state.update(cx, |state, cx| {
                            let index = state.workspaces.len() + 1;
                            let name = format!("Workspace {index}");
                            state.add_workspace(cx, &name);
                        });
                    })),
            )
            .children(
                workspaces
                    .into_iter()
                    .enumerate()
                    .map(|(ws_index, workspace)| {
                        let ws_id = workspace.id;
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
                            }));

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
                                let thread_dom_id = ws_index * 1000 + thread_index;

                                div()
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
                                    .child(thread.name)
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        this.app_state.update(cx, |state, cx| {
                                            state.set_active_thread(cx, thread_id);
                                        });
                                    }))
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
