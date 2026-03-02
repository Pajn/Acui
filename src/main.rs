#![recursion_limit = "1024"]

mod client;
mod domain;
mod state;
mod ui;

use gpui::{App, AppContext, Application, Bounds, WindowBounds, WindowOptions, px, size};
use state::AppState;
use ui::layout::WorkspaceLayout;

fn main() {
    Application::new().run(|cx: &mut App| {
        let app_state = cx.new(|cx| {
            let mut state = AppState::new();
            let ws_id = state.add_workspace(cx, "Workspace 1");
            let _ = state.add_thread(cx, ws_id, "Thread 1");
            state
        });

        let bounds = Bounds::centered(None, size(px(1100.0), px(740.0)), cx);
        cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                ..Default::default()
            },
            move |window, cx| cx.new(|cx| WorkspaceLayout::new(app_state.clone(), window, cx)),
        )
        .expect("failed to open window");

        cx.activate(true);
    });
}
