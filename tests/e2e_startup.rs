use std::fs;
use std::process::Command;
use std::thread;
use std::time::{Duration, Instant};

#[test]
fn app_runs_for_ten_seconds_with_agent_config() {
    let temp_dir = std::env::temp_dir().join(format!("acui-e2e-{}", uuid::Uuid::new_v4()));
    fs::create_dir_all(&temp_dir).expect("failed to create temp dir");

    let data_dir = temp_dir.join("data");
    let agent_config_path = temp_dir.join("acui_agent.toml");
    let app_config_path = temp_dir.join("acui.toml");

    fs::write(&agent_config_path, "command = \"cat\"\nargs = []\n")
        .expect("failed to write agent config");
    fs::write(
        &app_config_path,
        format!(
            "data_dir = \"{}\"\nagent_config = \"{}\"\n",
            data_dir.display(),
            agent_config_path.display()
        ),
    )
    .expect("failed to write app config");

    let mut child = Command::new(env!("CARGO_BIN_EXE_acui"))
        .current_dir(&temp_dir)
        .env("ACUI_HEADLESS", "1")
        .env("ACUI_E2E_OPEN_WINDOW", "1")
        .env("ACUI_E2E_DURATION_SECS", "10")
        .spawn()
        .expect("failed to launch app");

    let start = Instant::now();
    loop {
        if let Some(status) = child.try_wait().expect("failed to poll app process") {
            assert!(status.success(), "app exited with failure status: {status}");
            assert!(
                start.elapsed() >= Duration::from_secs(10),
                "app exited too early: {:?}",
                start.elapsed()
            );
            break;
        }

        if start.elapsed() > Duration::from_secs(20) {
            let _ = child.kill();
            let _ = child.wait();
            panic!("app did not exit within timeout window");
        }

        thread::sleep(Duration::from_millis(200));
    }

    let _ = fs::remove_dir_all(temp_dir);
}
