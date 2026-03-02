use super::ChatView;
use agent_client_protocol::{SessionConfigKind, SessionConfigOption, SessionConfigSelectOptions};
use gpui::prelude::*;
use gpui::*;

pub(super) fn render_config_option_row(
    cx: &Context<ChatView>,
    thread_id: uuid::Uuid,
    option_index: usize,
    option: SessionConfigOption,
) -> impl IntoElement {
    let option_id = option.id.to_string();
    let title = div().text_color(rgb(0xdddddd)).child(option.name);
    match option.kind {
        SessionConfigKind::Select(select) => {
            let entries = match select.options {
                SessionConfigSelectOptions::Ungrouped(values) => values
                    .into_iter()
                    .map(|entry| (entry.value.to_string(), entry.name))
                    .collect::<Vec<_>>(),
                SessionConfigSelectOptions::Grouped(groups) => groups
                    .into_iter()
                    .flat_map(|group| {
                        group.options.into_iter().map(move |entry| {
                            (
                                entry.value.to_string(),
                                format!("{} / {}", group.group, entry.name),
                            )
                        })
                    })
                    .collect::<Vec<_>>(),
                _ => Vec::new(),
            };
            let current_value = select.current_value.to_string();
            let value_buttons =
                entries
                    .into_iter()
                    .enumerate()
                    .map(|(value_index, (value_id, name))| {
                        let is_active = value_id == current_value;
                        div()
                            .id(("session-config-value", option_index * 100 + value_index))
                            .bg(if is_active {
                                rgb(0x0e639c)
                            } else {
                                rgb(0x3c3c3c)
                            })
                            .text_color(white())
                            .rounded_md()
                            .px_2()
                            .py_1()
                            .cursor_pointer()
                            .child(name)
                            .on_click(cx.listener({
                                let option_id = option_id.clone();
                                move |this, _, _, cx| {
                                    this.app_state.update(cx, |state, cx| {
                                        state.set_session_config_option(
                                            cx,
                                            thread_id,
                                            option_id.clone(),
                                            value_id.clone(),
                                        );
                                    });
                                }
                            }))
                    });
            div()
                .w_full()
                .flex()
                .flex_col()
                .gap_1()
                .child(title)
                .child(div().flex().gap_1().flex_wrap().children(value_buttons))
        }
        _ => div().w_full().child(title),
    }
}
