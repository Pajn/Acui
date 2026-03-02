use crate::state::AppState;
use agent_client_protocol::{PlanEntryPriority, PlanEntryStatus};
use gpui::prelude::*;
use gpui::*;

pub struct PlanSidebarView {
    app_state: Entity<AppState>,
}

impl PlanSidebarView {
    pub fn new(app_state: Entity<AppState>, cx: &mut Context<Self>) -> Self {
        cx.observe(&app_state, |_, _, cx| cx.notify()).detach();
        Self { app_state }
    }
}

impl Render for PlanSidebarView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let entries = self
            .app_state
            .read(cx)
            .active_thread_plan()
            .map(|plan| plan.entries)
            .unwrap_or_default();

        let rows = entries.into_iter().enumerate().map(|(index, entry)| {
            let priority = match entry.priority {
                PlanEntryPriority::High => "high",
                PlanEntryPriority::Medium => "medium",
                PlanEntryPriority::Low => "low",
                _ => "other",
            };
            let status = match entry.status {
                PlanEntryStatus::Pending => "pending",
                PlanEntryStatus::InProgress => "in_progress",
                PlanEntryStatus::Completed => "completed",
                _ => "other",
            };

            div()
                .id(("plan-entry", index))
                .p_2()
                .rounded_md()
                .bg(rgb(0x2d2d30))
                .flex()
                .flex_col()
                .gap_1()
                .child(div().text_color(white()).child(entry.content))
                .child(
                    div()
                        .text_xs()
                        .text_color(rgb(0xa0a0a0))
                        .child(format!("{priority} • {status}")),
                )
        });

        div()
            .id("plan-sidebar-root")
            .flex()
            .flex_col()
            .w(px(300.0))
            .h_full()
            .overflow_y_scroll()
            .bg(rgb(0x202225))
            .border_l_1()
            .border_color(rgb(0x3c3c3c))
            .p_3()
            .gap_2()
            .child(div().text_color(rgb(0xdddddd)).child("Plan"))
            .children(rows)
    }
}
