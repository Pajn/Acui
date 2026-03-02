use crate::state::AppState;
use crate::ui::chat::ChatView;
use crate::ui::sidebar::SidebarView;
use gpui::prelude::*;
use gpui::*;

pub struct WorkspaceLayout {
    sidebar: Entity<SidebarView>,
    chat: Entity<ChatView>,
}

impl WorkspaceLayout {
    pub fn new(app_state: Entity<AppState>, _window: &mut Window, cx: &mut Context<Self>) -> Self {
        let sidebar = cx.new(|cx| SidebarView::new(app_state.clone(), cx));
        let chat = cx.new(|cx| ChatView::new(app_state.clone(), cx));
        Self { sidebar, chat }
    }
}

impl Render for WorkspaceLayout {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .flex()
            .flex_row()
            .size_full()
            .bg(rgb(0x1e1e1e))
            .child(self.sidebar.clone())
            .child(self.chat.clone())
    }
}
