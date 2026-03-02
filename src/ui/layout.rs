use crate::state::AppState;
use crate::ui::chat::ChatView;
use crate::ui::sidebar::SidebarView;
use gpui::*;

pub struct WorkspaceLayout {
    sidebar: View<SidebarView>,
    chat: View<ChatView>,
}

impl WorkspaceLayout {
    pub fn build(app_state: Model<AppState>, cx: &mut WindowContext) -> View<Self> {
        cx.new_view(|cx| Self {
            sidebar: SidebarView::build(app_state.clone(), cx),
            chat: ChatView::build(app_state.clone(), cx),
        })
    }
}

impl Render for WorkspaceLayout {
    fn render(&mut self, _cx: &mut ViewContext<Self>) -> impl IntoElement {
        div()
            .flex()
            .flex_row()
            .size_full()
            .bg(rgb(0x1e1e1e)) // Dark mode background
            .child(self.sidebar.clone())
            .child(self.chat.clone())
    }
}
