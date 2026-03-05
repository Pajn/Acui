use crate::state::AppState;
use crate::ui::chat::ChatView;
use crate::ui::plan_sidebar::PlanSidebarView;
use crate::ui::sidebar::{SidebarDragItem, SidebarView};
use gpui::prelude::*;
use gpui::*;
use gpui_component::resizable::{h_resizable, resizable_panel};

pub struct WorkspaceLayout {
    app_state: Entity<AppState>,
    sidebar: Entity<SidebarView>,
    chat: Entity<ChatView>,
    plan_sidebar: Entity<PlanSidebarView>,
}

impl WorkspaceLayout {
    pub fn new(app_state: Entity<AppState>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let sidebar = cx.new(|cx| SidebarView::new(app_state.clone(), cx));
        let chat = cx.new(|cx| ChatView::new(app_state.clone(), window, cx));
        let plan_sidebar = cx.new(|cx| PlanSidebarView::new(app_state.clone(), cx));
        Self {
            app_state,
            sidebar,
            chat,
            plan_sidebar,
        }
    }

    #[doc(hidden)]
    #[allow(dead_code)]
    pub fn debug_chat_view(&self) -> Entity<ChatView> {
        self.chat.clone()
    }
}

impl Render for WorkspaceLayout {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let show_plan = self
            .app_state
            .read(cx)
            .active_thread_plan()
            .is_some_and(|plan| !plan.entries.is_empty());

        let main_area = div().flex_1().min_w(px(0.0)).h_full().child(
            h_resizable("workspace-main-resize")
                .child(
                    resizable_panel()
                        .size(px(260.0))
                        .size_range(px(180.0)..px(360.0))
                        .child(self.sidebar.clone()),
                )
                .child(resizable_panel().child(self.chat.clone())),
        );

        let root = div()
            .flex()
            .flex_row()
            .size_full()
            .bg(rgb(0x1e1e1e))
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, _: &MouseUpEvent, window, _cx| {
                    let sidebar = this.sidebar.clone();
                    window.on_next_frame(move |_, cx| {
                        sidebar.update(cx, |sidebar, cx| {
                            sidebar.clear_drag_feedback(cx);
                        });
                    });
                }),
            )
            .on_drop(cx.listener(|this, _: &SidebarDragItem, window, _cx| {
                let sidebar = this.sidebar.clone();
                window.on_next_frame(move |_, cx| {
                    sidebar.update(cx, |sidebar, cx| {
                        sidebar.clear_drag_feedback(cx);
                    });
                });
            }))
            .child(main_area);
        if show_plan {
            root.child(self.plan_sidebar.clone())
        } else {
            root
        }
    }
}
