#![recursion_limit = "1024"]

mod client;
mod domain;
mod state;
mod ui;

use gpui::{App, AppContext, Application, Bounds, WindowBounds, WindowOptions, px, size};
use state::AppState;
use ui::layout::WorkspaceLayout;

#[derive(Clone)]
struct GlobalAppState(gpui::Entity<AppState>);

impl gpui::Global for GlobalAppState {}

fn open_main_window(cx: &mut App, app_state: gpui::Entity<AppState>) {
    let bounds = Bounds::centered(None, size(px(1100.0), px(740.0)), cx);
    cx.open_window(
        WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(bounds)),
            ..Default::default()
        },
        move |window, cx| cx.new(|cx| WorkspaceLayout::new(app_state.clone(), window, cx)),
    )
    .expect("failed to open window");
}

fn main() {
    let application = Application::new();
    application.on_reopen(|cx: &mut App| {
        if cx.windows().is_empty()
            && let Some(global_state) = cx.try_global::<GlobalAppState>()
        {
            open_main_window(cx, global_state.0.clone());
        }
        cx.activate(true);
    });

    application.run(|cx: &mut App| {
        let app_state = cx.new(|cx| {
            let mut state = AppState::new();
            let ws_id = state.add_workspace(cx, "Workspace 1");
            let _ = state.add_thread(cx, ws_id, "Thread 1");
            state
        });
        cx.set_global(GlobalAppState(app_state.clone()));

        open_main_window(cx, app_state.clone());
        cx.activate(true);
    });
}
