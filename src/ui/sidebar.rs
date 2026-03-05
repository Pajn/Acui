use crate::state::AppState;
use chrono::{DateTime, Utc};
use gpui::prelude::*;
use gpui::*;
use gpui_component::input::{Input, InputEvent, InputState};
use gpui_component::menu::{ContextMenuExt, PopupMenuItem};
use std::collections::{HashMap, HashSet};
use std::time::Duration;

const THREAD_DROP_GAP_HEIGHT: f32 = 30.0;
const THREAD_DROP_GAP_MARGIN_BOTTOM: f32 = 4.0;
const WORKSPACE_HEADER_HEIGHT: f32 = 34.0;
const WORKSPACE_TOP_MARGIN_HEIGHT: f32 = 8.0;
const WORKSPACE_NEW_THREAD_HEIGHT: f32 = 22.0;

pub struct SidebarView {
    app_state: Entity<AppState>,
    collapsed_workspaces: HashSet<uuid::Uuid>,
    renaming_thread_id: Option<uuid::Uuid>,
    rename_input: Option<Entity<InputState>>,
    dragging_item: Option<SidebarDragItem>,
    drag_placeholder: Option<SidebarDropPlaceholder>,
    drop_animation: Option<SidebarDropAnimation>,
    drop_animation_nonce: u64,
}

struct SidebarThreadEntry {
    id: uuid::Uuid,
    name: String,
    updated_at: DateTime<Utc>,
    can_fork: bool,
    has_unread_stop: bool,
}

struct SidebarWorkspaceEntry {
    id: uuid::Uuid,
    name: String,
    threads: Vec<SidebarThreadEntry>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum SidebarDragItem {
    Workspace(uuid::Uuid),
    Thread {
        workspace_id: uuid::Uuid,
        thread_id: uuid::Uuid,
    },
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum SidebarDropPlaceholder {
    Workspace {
        dragged_workspace_id: uuid::Uuid,
        insertion_index: usize,
    },
    Thread {
        workspace_id: uuid::Uuid,
        dragged_thread_id: uuid::Uuid,
        insertion_index: usize,
    },
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum SidebarDropAnimation {
    Workspace(uuid::Uuid),
    Thread {
        workspace_id: uuid::Uuid,
        thread_id: uuid::Uuid,
    },
}

struct DragPreview {
    kind: DragPreviewKind,
}

enum DragPreviewKind {
    Thread {
        name: SharedString,
    },
    Workspace {
        name: SharedString,
        is_collapsed: bool,
        thread_names: Vec<SharedString>,
    },
}

impl DragPreview {
    fn thread(name: impl Into<SharedString>) -> Self {
        Self {
            kind: DragPreviewKind::Thread { name: name.into() },
        }
    }

    fn workspace(
        name: impl Into<SharedString>,
        is_collapsed: bool,
        thread_names: Vec<SharedString>,
    ) -> Self {
        Self {
            kind: DragPreviewKind::Workspace {
                name: name.into(),
                is_collapsed,
                thread_names,
            },
        }
    }
}

impl Render for DragPreview {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        match &self.kind {
            DragPreviewKind::Thread { name } => div().w(px(240.0)).text_color(rgb(0xcccccc)).child(
                div()
                    .w_full()
                    .min_w_0()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .overflow_hidden()
                            .whitespace_nowrap()
                            .text_ellipsis()
                            .child(name.clone()),
                    ),
            ),
            DragPreviewKind::Workspace {
                name,
                is_collapsed,
                thread_names,
            } => div()
                .w(px(260.0))
                .text_color(rgb(0xcccccc))
                .child(if *is_collapsed {
                    format!("▶ {name}")
                } else {
                    format!("▼ {name}")
                })
                .when(!is_collapsed, |this| {
                    this.child(
                        div()
                            .text_color(rgb(0x8f8f8f))
                            .text_sm()
                            .pt_1()
                            .child("+ New Thread"),
                    )
                    .children(thread_names.iter().take(6).map(|thread_name| {
                        div()
                            .text_color(rgb(0xcccccc))
                            .overflow_hidden()
                            .whitespace_nowrap()
                            .text_ellipsis()
                            .child(thread_name.clone())
                    }))
                }),
        }
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
            renaming_thread_id: None,
            rename_input: None,
            dragging_item: None,
            drag_placeholder: None,
            drop_animation: None,
            drop_animation_nonce: 0,
        }
    }

    fn set_drag_placeholder(
        &mut self,
        placeholder: Option<SidebarDropPlaceholder>,
        cx: &mut Context<Self>,
    ) {
        if self.drag_placeholder != placeholder {
            self.drag_placeholder = placeholder;
            cx.notify();
        }
    }

    fn begin_drag(
        &mut self,
        dragged_item: SidebarDragItem,
        placeholder: Option<SidebarDropPlaceholder>,
        cx: &mut Context<Self>,
    ) {
        let mut changed = false;
        if self.dragging_item != Some(dragged_item) {
            self.dragging_item = Some(dragged_item);
            changed = true;
        }
        if self.drag_placeholder != placeholder {
            self.drag_placeholder = placeholder;
            changed = true;
        }
        if changed {
            cx.notify();
        }
    }

    pub fn clear_drag_feedback(&mut self, cx: &mut Context<Self>) {
        if self.dragging_item.is_some() || self.drag_placeholder.is_some() {
            self.dragging_item = None;
            self.drag_placeholder = None;
            cx.notify();
        }
    }

    fn prepare_drop_feedback(&mut self, animation_target: SidebarDropAnimation) {
        self.dragging_item = None;
        self.drag_placeholder = None;
        self.drop_animation = Some(animation_target);
        self.drop_animation_nonce = self.drop_animation_nonce.wrapping_add(1);
    }

    fn commit_pending_drop(&mut self, cx: &mut Context<Self>) {
        match (self.dragging_item, self.drag_placeholder) {
            (
                Some(SidebarDragItem::Workspace(dragged_workspace_id)),
                Some(SidebarDropPlaceholder::Workspace {
                    dragged_workspace_id: placeholder_dragged_workspace_id,
                    insertion_index,
                }),
            ) if placeholder_dragged_workspace_id == dragged_workspace_id => {
                self.prepare_drop_feedback(SidebarDropAnimation::Workspace(dragged_workspace_id));
                self.app_state.update(cx, |state, cx| {
                    state.reorder_workspaces_to_index(cx, dragged_workspace_id, insertion_index);
                });
            }
            (
                Some(SidebarDragItem::Thread {
                    workspace_id,
                    thread_id: dragged_thread_id,
                }),
                Some(SidebarDropPlaceholder::Thread {
                    workspace_id: placeholder_workspace_id,
                    dragged_thread_id: placeholder_dragged_thread_id,
                    insertion_index,
                }),
            ) if placeholder_workspace_id == workspace_id
                && placeholder_dragged_thread_id == dragged_thread_id =>
            {
                self.prepare_drop_feedback(SidebarDropAnimation::Thread {
                    workspace_id,
                    thread_id: dragged_thread_id,
                });
                self.app_state.update(cx, |state, cx| {
                    state.reorder_threads_to_index(
                        cx,
                        workspace_id,
                        dragged_thread_id,
                        insertion_index,
                    );
                });
            }
            _ => self.clear_drag_feedback(cx),
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
            let active_thread_id = state.active_thread_id;
            let workspaces: Vec<SidebarWorkspaceEntry> = state
                .workspaces
                .iter()
                .map(|workspace| SidebarWorkspaceEntry {
                    id: workspace.id,
                    name: workspace.name.clone(),
                    threads: workspace
                        .threads
                        .iter()
                        .map(|thread| {
                            let thread_id = thread.id;
                            SidebarThreadEntry {
                                id: thread_id,
                                name: thread.name.clone(),
                                updated_at: thread.updated_at,
                                can_fork: state.thread_can_fork(thread_id),
                                has_unread_stop: active_thread_id != Some(thread_id)
                                    && state.thread_has_unread_stop(thread_id),
                            }
                        })
                        .collect(),
                })
                .collect();
            (workspaces, active_thread_id)
        };
        let workspace_thread_counts: HashMap<uuid::Uuid, usize> = workspaces
            .iter()
            .map(|workspace| (workspace.id, workspace.threads.len()))
            .collect();
        let workspace_visible_height = |workspace_id: uuid::Uuid| -> f32 {
            let thread_count = workspace_thread_counts
                .get(&workspace_id)
                .copied()
                .unwrap_or(0) as f32;
            if self.collapsed_workspaces.contains(&workspace_id) {
                WORKSPACE_TOP_MARGIN_HEIGHT + WORKSPACE_HEADER_HEIGHT
            } else {
                WORKSPACE_TOP_MARGIN_HEIGHT
                    + WORKSPACE_HEADER_HEIGHT
                    + WORKSPACE_NEW_THREAD_HEIGHT
                    + thread_count * THREAD_DROP_GAP_HEIGHT
            }
        };

        div()
            .id("sidebar-root")
            .flex()
            .flex_col()
            .w_full()
            .h_full()
            .overflow_y_scroll()
            .bg(rgb(0x252526))
            .border_r_1()
            .border_color(rgb(0x3c3c3c))
            .p_3()
            .gap_2()
            .on_drop(cx.listener(|this, _: &SidebarDragItem, _, cx| {
                this.commit_pending_drop(cx);
            }))
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
            .children({
                let sidebar = cx.entity();
                let dragging_workspace_id = match self.dragging_item {
                    Some(SidebarDragItem::Workspace(workspace_id)) => Some(workspace_id),
                    _ => None,
                };
                let workspace_insertion_index = match self.drag_placeholder {
                    Some(SidebarDropPlaceholder::Workspace {
                        insertion_index, ..
                    }) => Some(insertion_index),
                    _ => None,
                };
                let workspace_gap_height = dragging_workspace_id
                    .map(workspace_visible_height)
                    .unwrap_or(WORKSPACE_TOP_MARGIN_HEIGHT + WORKSPACE_HEADER_HEIGHT);
                let workspaces: Vec<SidebarWorkspaceEntry> =
                    if let Some(dragged_workspace_id) = dragging_workspace_id {
                        workspaces
                            .into_iter()
                            .filter(|workspace| workspace.id != dragged_workspace_id)
                            .collect()
                    } else {
                        workspaces
                    };
                let visible_workspace_count = workspaces.len();
                let mut workspace_elements: Vec<AnyElement> = Vec::new();

                for (ws_index, workspace) in workspaces.into_iter().enumerate() {
                    let ws_id = workspace.id;
                    let workspace_name = workspace.name.clone();
                    let is_collapsed = self.collapsed_workspaces.contains(&ws_id);
                    let workspace_preview_threads: Vec<SharedString> = workspace
                        .threads
                        .iter()
                        .map(|thread| thread.name.clone().into())
                        .collect();
                    let drop_animation_nonce = self.drop_animation_nonce;
                    let animate_workspace_drop = matches!(
                        self.drop_animation,
                        Some(SidebarDropAnimation::Workspace(dropped_workspace_id))
                            if dropped_workspace_id == ws_id
                    );

                    if workspace_insertion_index == Some(ws_index) {
                        let gap_insertion_index = ws_index;
                        workspace_elements.push(
                            div()
                                .w_full()
                                .on_drop(cx.listener(move |this, _: &SidebarDragItem, _, cx| {
                                    let _ = gap_insertion_index;
                                    this.commit_pending_drop(cx);
                                }))
                                .child(render_drop_gap(px(workspace_gap_height), px(0.0)))
                                .into_any_element(),
                        );
                    }

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
                        .on_drag(SidebarDragItem::Workspace(ws_id), {
                            let sidebar = sidebar.clone();
                            move |_dragged, _, _, cx| {
                                sidebar.update(cx, |this, cx| {
                                    let insertion_index = {
                                        let state = this.app_state.read(cx);
                                        state
                                            .workspaces
                                            .iter()
                                            .position(|workspace| workspace.id == ws_id)
                                            .unwrap_or(0)
                                    };
                                    this.begin_drag(
                                        SidebarDragItem::Workspace(ws_id),
                                        Some(SidebarDropPlaceholder::Workspace {
                                            dragged_workspace_id: ws_id,
                                            insertion_index,
                                        }),
                                        cx,
                                    );
                                });

                                let preview_name = workspace_name.clone();
                                let preview_is_collapsed = is_collapsed;
                                let preview_threads = workspace_preview_threads.clone();
                                cx.new(|_| {
                                    DragPreview::workspace(
                                        preview_name,
                                        preview_is_collapsed,
                                        preview_threads,
                                    )
                                })
                            }
                        });

                    let content = if is_collapsed {
                        div().child(header).into_any_element()
                    } else {
                        let dragging_thread_id = match self.dragging_item {
                            Some(SidebarDragItem::Thread {
                                workspace_id,
                                thread_id,
                            }) if workspace_id == ws_id => Some(thread_id),
                            _ => None,
                        };
                        let thread_insertion_index = match self.drag_placeholder {
                            Some(SidebarDropPlaceholder::Thread {
                                workspace_id,
                                insertion_index,
                                ..
                            }) if workspace_id == ws_id => Some(insertion_index),
                            _ => None,
                        };
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

                        let threads: Vec<SidebarThreadEntry> =
                            if let Some(dragged_thread_id) = dragging_thread_id {
                                workspace
                                    .threads
                                    .into_iter()
                                    .filter(|thread| thread.id != dragged_thread_id)
                                    .collect()
                            } else {
                                workspace.threads
                            };
                        let visible_thread_count = threads.len();
                        let mut thread_elements: Vec<AnyElement> = Vec::new();

                        for (thread_index, thread) in threads.into_iter().enumerate() {
                            let thread_id = thread.id;
                            let is_active = active_thread_id == Some(thread_id);
                            let has_unread_stop = thread.has_unread_stop;
                            let thread_dom_id = ws_index * 1000 + thread_index;
                            let thread_name = thread.name.clone();
                            let thread_drag_name = thread_name.clone();
                            let rename_thread_name = thread.name.clone();
                            let is_renaming = self.renaming_thread_id == Some(thread_id);
                            let rename_input = self.rename_input.clone();
                            let can_fork = thread.can_fork;
                            let animate_thread_drop = matches!(
                                self.drop_animation,
                                Some(SidebarDropAnimation::Thread {
                                    workspace_id,
                                    thread_id: dropped_thread_id,
                                }) if workspace_id == ws_id && dropped_thread_id == thread_id
                            );

                            if thread_insertion_index == Some(thread_index) {
                                let gap_insertion_index = thread_index;
                                thread_elements.push(
                                    div()
                                        .w_full()
                                        .on_drop(cx.listener(
                                            move |this, _: &SidebarDragItem, _, cx| {
                                                let _ = gap_insertion_index;
                                                this.commit_pending_drop(cx);
                                            },
                                        ))
                                        .child(render_drop_gap(
                                            px(THREAD_DROP_GAP_HEIGHT),
                                            px(THREAD_DROP_GAP_MARGIN_BOTTOM),
                                        ))
                                        .into_any_element(),
                                );
                            }

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
                                .w_full()
                                .min_w_0()
                                .pl_4()
                                .pr_2()
                                .py_1()
                                .rounded_sm()
                                .bg(if is_active {
                                    rgb(0x37373d)
                                } else {
                                    rgba(0x00000000)
                                })
                                .when(thread_dom_id == 0, |this| {
                                    this.debug_selector(|| "sidebar-thread-row-0".to_string())
                                })
                                .text_color(rgb(0xcccccc))
                                .cursor_pointer()
                                .when(!is_renaming, |this| {
                                    this.on_drag(
                                        SidebarDragItem::Thread {
                                            workspace_id: ws_id,
                                            thread_id,
                                        },
                                        {
                                            let sidebar = sidebar.clone();
                                            move |_dragged, _, _, cx| {
                                                sidebar.update(cx, |this, cx| {
                                                    let insertion_index = {
                                                        let state = this.app_state.read(cx);
                                                        state
                                                            .workspaces
                                                            .iter()
                                                            .find(|workspace| workspace.id == ws_id)
                                                            .and_then(|workspace| {
                                                                workspace.threads.iter().position(
                                                                    |thread| thread.id == thread_id,
                                                                )
                                                            })
                                                            .unwrap_or(0)
                                                    };
                                                    this.begin_drag(
                                                        SidebarDragItem::Thread {
                                                            workspace_id: ws_id,
                                                            thread_id,
                                                        },
                                                        Some(SidebarDropPlaceholder::Thread {
                                                            workspace_id: ws_id,
                                                            dragged_thread_id: thread_id,
                                                            insertion_index,
                                                        }),
                                                        cx,
                                                    );
                                                });

                                                let preview_name = thread_drag_name.clone();
                                                cx.new(|_| DragPreview::thread(preview_name))
                                            }
                                        },
                                    )
                                })
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.app_state.update(cx, |state, cx| {
                                        state.set_active_thread(cx, thread_id);
                                    });
                                }))
                                .child(
                                    div()
                                        .w_full()
                                        .min_w_0()
                                        .flex()
                                        .items_center()
                                        .gap_2()
                                        .child(if is_renaming {
                                            if let Some(input) = rename_input {
                                                div()
                                                    .flex_1()
                                                    .min_w_0()
                                                    .child(Input::new(&input))
                                                    .into_any_element()
                                            } else {
                                                div()
                                                    .flex_1()
                                                    .min_w_0()
                                                    .overflow_hidden()
                                                    .whitespace_nowrap()
                                                    .text_ellipsis()
                                                    .child(thread_name.clone())
                                                    .into_any_element()
                                            }
                                        } else {
                                            div()
                                                .flex_1()
                                                .min_w_0()
                                                .overflow_hidden()
                                                .whitespace_nowrap()
                                                .text_ellipsis()
                                                .child(thread_name.clone())
                                                .into_any_element()
                                        })
                                        .child(div().flex_shrink_0().child(trailing)),
                                );

                            let row = if is_renaming {
                                row.into_any_element()
                            } else {
                                row.context_menu({
                                    let sidebar = sidebar.clone();
                                    let rename_thread_name = rename_thread_name.clone();
                                    move |menu, _, _| {
                                        let mut menu =
                                            menu.item(PopupMenuItem::new("Rename").on_click({
                                                let sidebar = sidebar.clone();
                                                let rename_thread_name = rename_thread_name.clone();
                                                move |_, window, cx| {
                                                    sidebar.update(cx, |this, cx| {
                                                        this.begin_rename(
                                                            thread_id,
                                                            rename_thread_name.clone(),
                                                            window,
                                                            cx,
                                                        );
                                                    });
                                                }
                                            }));
                                        if can_fork {
                                            menu =
                                                menu.item(PopupMenuItem::new("Fork").on_click({
                                                    let sidebar = sidebar.clone();
                                                    move |_, _, cx| {
                                                        sidebar.update(cx, |this, cx| {
                                                            this.app_state.update(
                                                                cx,
                                                                |state, cx| {
                                                                    state
                                                                        .fork_thread(cx, thread_id);
                                                                },
                                                            );
                                                        });
                                                    }
                                                }));
                                        }
                                        menu.item(PopupMenuItem::new("Delete thread").on_click({
                                            let sidebar = sidebar.clone();
                                            move |_, _, cx| {
                                                sidebar.update(cx, |this, cx| {
                                                    this.app_state.update(cx, |state, cx| {
                                                        let _ = state.delete_thread(cx, thread_id);
                                                    });
                                                });
                                            }
                                        }))
                                        .item(
                                            PopupMenuItem::new("Mark as unread").on_click({
                                                let sidebar = sidebar.clone();
                                                move |_, _, cx| {
                                                    sidebar.update(cx, |this, cx| {
                                                        this.app_state.update(cx, |state, cx| {
                                                            state.mark_thread_unread(cx, thread_id);
                                                        });
                                                    });
                                                }
                                            }),
                                        )
                                    }
                                })
                                .into_any_element()
                            };

                            let row_target = div()
                                .w_full()
                                .on_drag_move::<SidebarDragItem>(cx.listener(
                                    move |this, event: &DragMoveEvent<SidebarDragItem>, _, cx| {
                                        if !event.bounds.contains(&event.event.position) {
                                            return;
                                        }
                                        let SidebarDragItem::Thread {
                                            workspace_id,
                                            thread_id: dragged_thread_id,
                                        } = event.drag(cx)
                                        else {
                                            return;
                                        };
                                        if *workspace_id != ws_id || *dragged_thread_id == thread_id
                                        {
                                            return;
                                        }

                                        let midpoint =
                                            event.bounds.origin.y + event.bounds.size.height * 0.5;
                                        let insertion_index = if event.event.position.y < midpoint {
                                            thread_index
                                        } else {
                                            thread_index + 1
                                        };
                                        this.set_drag_placeholder(
                                            Some(SidebarDropPlaceholder::Thread {
                                                workspace_id: *workspace_id,
                                                dragged_thread_id: *dragged_thread_id,
                                                insertion_index,
                                            }),
                                            cx,
                                        );
                                    },
                                ))
                                .on_drop(cx.listener(move |this, _: &SidebarDragItem, _, cx| {
                                    let _ = thread_index;
                                    this.commit_pending_drop(cx);
                                }))
                                .child(row);

                            if animate_thread_drop {
                                thread_elements.push(
                                    row_target
                                        .with_animation(
                                            ("sidebar-drop-thread", drop_animation_nonce),
                                            Animation::new(Duration::from_millis(110))
                                                .with_easing(ease_in_out),
                                            |this, delta| this.opacity(0.7 + 0.3 * delta),
                                        )
                                        .into_any_element(),
                                );
                            } else {
                                thread_elements.push(row_target.into_any_element());
                            }
                        }

                        if thread_insertion_index == Some(visible_thread_count) {
                            let gap_insertion_index = visible_thread_count;
                            thread_elements.push(
                                div()
                                    .w_full()
                                    .on_drop(cx.listener(
                                        move |this, _: &SidebarDragItem, _, cx| {
                                            let _ = gap_insertion_index;
                                            this.commit_pending_drop(cx);
                                        },
                                    ))
                                    .child(render_drop_gap(
                                        px(THREAD_DROP_GAP_HEIGHT),
                                        px(THREAD_DROP_GAP_MARGIN_BOTTOM),
                                    ))
                                    .into_any_element(),
                            );
                        }

                        div()
                            .child(header)
                            .child(new_thread_button)
                            .children(thread_elements)
                            .into_any_element()
                    };

                    let workspace_target = div()
                        .w_full()
                        .on_drag_move::<SidebarDragItem>(cx.listener(
                            move |this, event: &DragMoveEvent<SidebarDragItem>, _, cx| {
                                if !event.bounds.contains(&event.event.position) {
                                    return;
                                }
                                let SidebarDragItem::Workspace(dragged_workspace_id) =
                                    event.drag(cx)
                                else {
                                    return;
                                };
                                if *dragged_workspace_id == ws_id {
                                    return;
                                }

                                let midpoint =
                                    event.bounds.origin.y + event.bounds.size.height * 0.5;
                                let insertion_index = if event.event.position.y < midpoint {
                                    ws_index
                                } else {
                                    ws_index + 1
                                };
                                this.set_drag_placeholder(
                                    Some(SidebarDropPlaceholder::Workspace {
                                        dragged_workspace_id: *dragged_workspace_id,
                                        insertion_index,
                                    }),
                                    cx,
                                );
                            },
                        ))
                        .on_drop(cx.listener(move |this, _: &SidebarDragItem, _, cx| {
                            let _ = ws_index;
                            this.commit_pending_drop(cx);
                        }))
                        .child(content);

                    if animate_workspace_drop {
                        workspace_elements.push(
                            workspace_target
                                .with_animation(
                                    ("sidebar-drop-workspace", drop_animation_nonce),
                                    Animation::new(Duration::from_millis(130))
                                        .with_easing(ease_in_out),
                                    |this, delta| this.opacity(0.7 + 0.3 * delta),
                                )
                                .into_any_element(),
                        );
                    } else {
                        workspace_elements.push(workspace_target.into_any_element());
                    }
                }

                if workspace_insertion_index == Some(visible_workspace_count) {
                    let gap_insertion_index = visible_workspace_count;
                    workspace_elements.push(
                        div()
                            .w_full()
                            .on_drop(cx.listener(move |this, _: &SidebarDragItem, _, cx| {
                                let _ = gap_insertion_index;
                                this.commit_pending_drop(cx);
                            }))
                            .child(render_drop_gap(px(workspace_gap_height), px(0.0)))
                            .into_any_element(),
                    );
                }

                workspace_elements
            })
    }
}

fn render_drop_gap(height: Pixels, margin_bottom: Pixels) -> AnyElement {
    div()
        .w_full()
        .h(height)
        .mb(margin_bottom)
        .rounded_sm()
        .border_1()
        .border_color(hsla(0.56, 0.82, 0.34, 0.6))
        .bg(hsla(0.56, 0.82, 0.34, 0.12))
        .with_animation(
            "sidebar-drop-gap-pulse",
            Animation::new(Duration::from_millis(850))
                .repeat()
                .with_easing(ease_in_out),
            |this, delta| {
                let pulse = 0.10 + 0.14 * (1.0 - (2.0 * delta - 1.0).abs());
                this.bg(hsla(0.56, 0.82, 0.34, pulse))
            },
        )
        .into_any_element()
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
