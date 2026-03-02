#![recursion_limit = "1024"]

mod client;
mod config;
mod domain;
mod persistence;
mod state;
mod ui;

use config::AppConfig;
use gpui::{App, AppContext, Application, Bounds, WindowBounds, WindowOptions, px, size};
use state::AppState;
use std::time::Duration;
use ui::layout::WorkspaceLayout;

#[derive(Clone)]
struct GlobalAppState(gpui::Entity<AppState>);

impl gpui::Global for GlobalAppState {}

fn open_main_window(cx: &mut App, app_state: gpui::Entity<AppState>) {
    let bounds = Bounds::centered(None, size(px(1100.0), px(740.0)), cx);
    cx.open_window(
        WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(bounds)),
            app_id: Some("acui".into()),
            ..Default::default()
        },
        move |window, cx| {
            window.set_app_id("acui");
            window.set_window_title("acui");
            cx.new(|cx| WorkspaceLayout::new(app_state.clone(), window, cx))
        },
    )
    .expect("failed to open window");
}

fn main() {
    let e2e_duration_secs = std::env::var("ACUI_E2E_DURATION_SECS")
        .ok()
        .and_then(|value| value.parse::<u64>().ok());
    let headless =
        std::env::var("ACUI_HEADLESS").as_deref() == Ok("1") || e2e_duration_secs.is_some();
    let application = if headless {
        Application::headless()
    } else {
        Application::new()
    };
    application.on_reopen(|cx: &mut App| {
        if cx.windows().is_empty()
            && let Some(global_state) = cx.try_global::<GlobalAppState>()
        {
            open_main_window(cx, global_state.0.clone());
        }
        cx.activate(true);
    });

    application.run(move |cx: &mut App| {
        let app_state = cx.new(|cx| {
            let config = AppConfig::load().unwrap_or_else(|err| {
                eprintln!("failed to load acui.toml, using defaults: {err}");
                AppConfig::default()
            });
            let mut state = AppState::new_with_config(config);
            if let Err(err) = state.restore_persisted_state(cx) {
                eprintln!("failed to restore persisted state: {err}");
            }
            if let Some(active_thread_id) = state.active_thread_id {
                state.set_active_thread(cx, active_thread_id);
            }
            if state.workspaces.is_empty() {
                let workspace_path = std::env::current_dir().unwrap_or_else(|_| ".".into());
                let ws_id = state.add_workspace_from_path(cx, workspace_path);
                let _ = state.add_thread(cx, ws_id, "Thread 1");
            }
            state
        });
        cx.set_global(GlobalAppState(app_state.clone()));

        if !headless {
            open_main_window(cx, app_state.clone());
        }

        if let Some(duration_secs) = e2e_duration_secs {
            let background = cx.background_executor().clone();
            cx.spawn(move |cx: &mut gpui::AsyncApp| {
                let cx = cx.clone();
                async move {
                    background.timer(Duration::from_secs(duration_secs)).await;
                    let _ = cx.update(|cx: &mut App| cx.quit());
                }
            })
            .detach();
        }

        cx.activate(true);
    });
}
