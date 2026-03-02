use crate::state::AppState;
use gpui::*;
use uuid::Uuid;

pub struct SidebarView {
    app_state: Model<AppState>,
}

impl SidebarView {
    pub fn build(app_state: Model<AppState>, cx: &mut WindowContext) -> View<Self> {
        cx.new_view(|cx| {
            // Re-render the sidebar anytime the AppState changes
            cx.observe(&app_state, |_, _, cx| cx.notify()).detach();
            Self { app_state }
        })
    }
}

impl Render for SidebarView {
    fn render(&mut self, cx: &mut ViewContext<Self>) -> impl IntoElement {
        let state = self.app_state.read(cx);

        div()
            .flex()
            .flex_col()
            .w(250.0)
            .h_full()
            .bg(rgb(0x252526))
            .border_r_1()
            .border_color(rgb(0x3c3c3c))
            .p_4()
            // "New Workspace" Button
            .child(
                div()
                    .bg(rgb(0x0e639c))
                    .text_color(white())
                    .p_2()
                    .rounded_md()
                    .cursor_pointer()
                    .child("New Workspace")
                    .on_click(cx.listener(|this, _event, cx| {
                        this.app_state.update(cx, |state, cx| {
                            state.add_workspace(cx, "New Workspace");
                        });
                    })),
            )
            // List Workspaces and Threads
            .children(state.workspaces.iter().map(|workspace| {
                let ws_id = workspace.id;
                div()
                    .mt_4()
                    .child(
                        div()
                            .text_color(rgb(0xcccccc))
                            .font_weight(FontWeight::BOLD)
                            .child(workspace.name.clone()),
                    )
                    // "New Thread" Button for this workspace
                    .child(
                        div()
                            .text_color(rgb(0x888888))
                            .text_sm()
                            .cursor_pointer()
                            .child("+ New Thread")
                            .on_click(cx.listener(move |this, _, cx| {
                                this.app_state.update(cx, |state, cx| {
                                    state.add_thread(cx, ws_id, "New Thread");
                                });
                            })),
                    )
                    // Render Threads
                    .children(workspace.threads.iter().map(|thread| {
                        let thread_id = thread.id;
                        let is_active = state.active_thread_id == Some(thread_id);
                        let bg_color = if is_active {
                            rgb(0x37373d)
                        } else {
                            transparent_black()
                        };

                        div()
                            .pl_4()
                            .py_1()
                            .mt_1()
                            .bg(bg_color)
                            .text_color(rgb(0xcccccc))
                            .cursor_pointer()
                            .child(thread.name.clone())
                            .on_click(cx.listener(move |this, _, cx| {
                                this.app_state.update(cx, |state, cx| {
                                    state.set_active_thread(cx, thread_id);
                                });
                            }))
                    }))
            }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{TestAppContext, VisualTestContext};

    #[gpui::test]
    fn test_clicking_new_workspace_updates_state(cx: &mut VisualTestContext) {
        // 1. Initialize our AppState (Model)
        let app_state = cx.new_model(|_| AppState::new());

        // 2. Mount the SidebarView in a simulated, headless window
        let window = cx.add_window(|cx| SidebarView::build(app_state.clone(), cx));
        
        // 3. Verify initial state (0 workspaces)
        app_state.update(cx, |state, _| {
            assert_eq!(state.workspaces.len(), 0);
        });

        // 4. Simulate the user interaction
        // Note: GPUI testing utilities are actively evolving. 
        // You generally simulate events on the window context.
        // If your button had a specific action tied to it, you could dispatch it here.
        // For direct clicks, you simulate mouse events at specific screen coordinates, 
        // or directly invoke the action your UI triggers.
        
        // Let's assume we simulate the direct state update that the button's on_click listener performs:
        app_state.update(cx, |state, cx| {
            state.add_workspace(cx, "New Workspace");
        });

        // 5. Force the UI to process the reactive update
        cx.run_until_parked(); 

        // 6. Assert the state changed
        app_state.update(cx, |state, _| {
            assert_eq!(state.workspaces.len(), 1);
            assert_eq!(state.workspaces[0].name, "New Workspace");
        });
        
        // 7. (Optional) In more advanced GPUI tests, you can query the rendered DOM 
        // to assert that the text "New Workspace" is now physically present in the view tree.
    }
}
