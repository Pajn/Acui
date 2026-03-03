use crate::state::AppState;
use crate::ui::chat::ChatView;
use crate::ui::plan_sidebar::PlanSidebarView;
use crate::ui::sidebar::SidebarView;
use gpui::prelude::*;
use gpui::*;

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
}

impl Render for WorkspaceLayout {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let show_plan = self
            .app_state
            .read(cx)
            .active_thread_plan()
            .is_some_and(|plan| !plan.entries.is_empty());
        let root = div()
            .flex()
            .flex_row()
            .size_full()
            .bg(rgb(0x1e1e1e))
            .child(self.sidebar.clone())
            .child(self.chat.clone());
        if show_plan {
            root.child(self.plan_sidebar.clone())
        } else {
            root
        }
    }
}
