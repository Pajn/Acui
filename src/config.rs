use serde::Deserialize;
use std::path::PathBuf;

#[derive(Debug, Clone)]
pub struct AppConfig {
    pub data_dir: PathBuf,
    pub agent_config: Option<PathBuf>,
    pub log_file: Option<PathBuf>,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            data_dir: default_data_dir(),
            agent_config: None,
            log_file: None,
        }
    }
}

#[derive(Debug, Deserialize, Default)]
struct AppConfigFile {
    data_dir: Option<PathBuf>,
    agent_config: Option<PathBuf>,
    log_file: Option<PathBuf>,
}

impl AppConfig {
    pub fn load() -> anyhow::Result<Self> {
        let path = PathBuf::from("acui.toml");
        let parsed: AppConfigFile = if path.exists() {
            let raw = std::fs::read_to_string(path)?;
            toml::from_str(&raw)?
        } else {
            AppConfigFile::default()
        };

        Ok(Self {
            data_dir: parsed.data_dir.unwrap_or_else(default_data_dir),
            agent_config: parsed.agent_config,
            log_file: parsed.log_file,
        })
    }
}

fn default_data_dir() -> PathBuf {
    directories::ProjectDirs::from("", "", "acui")
        .map(|dirs| dirs.data_dir().to_path_buf())
        .unwrap_or_else(|| PathBuf::from(".acui"))
}
