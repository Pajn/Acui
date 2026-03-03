use serde::Deserialize;
use std::path::PathBuf;

/// Configuration for a single ACP agent process.
#[derive(Debug, Clone, Deserialize)]
pub struct AgentConfig {
    pub name: String,
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub cwd: Option<PathBuf>,
}

#[derive(Debug, Clone)]
pub struct AppConfig {
    pub data_dir: PathBuf,
    /// Configured real agents, in the order they appear in `acui.toml`.
    pub agents: Vec<AgentConfig>,
    /// When `true` (the default), a built-in mock agent is always available.
    pub enable_mock_agent: bool,
    pub log_file: Option<PathBuf>,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            data_dir: default_data_dir(),
            agents: Vec::new(),
            enable_mock_agent: true,
            log_file: None,
        }
    }
}

#[derive(Debug, Deserialize, Default)]
struct AppConfigFile {
    data_dir: Option<PathBuf>,
    #[serde(default, rename = "agent")]
    agents: Vec<AgentConfig>,
    enable_mock_agent: Option<bool>,
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
            agents: parsed.agents,
            enable_mock_agent: parsed.enable_mock_agent.unwrap_or(true),
            log_file: parsed.log_file,
        })
    }
}

fn default_data_dir() -> PathBuf {
    directories::ProjectDirs::from("", "", "acui")
        .map(|dirs| dirs.data_dir().to_path_buf())
        .unwrap_or_else(|| PathBuf::from(".acui"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config_enables_mock_agent() {
        let config = AppConfig::default();
        assert!(config.enable_mock_agent);
        assert!(config.agents.is_empty());
    }

    #[test]
    fn config_parses_agent_tables() {
        let toml = r#"
enable_mock_agent = false

[[agent]]
name = "copilot"
command = "copilot"
args = ["--acp"]

[[agent]]
name = "gemini"
command = "gemini"
args = ["--experimental-acp"]
"#;
        let file: AppConfigFile = toml::from_str(toml).expect("should parse");
        assert_eq!(file.agents.len(), 2);
        assert_eq!(file.agents[0].name, "copilot");
        assert_eq!(file.agents[1].name, "gemini");
        assert_eq!(file.enable_mock_agent, Some(false));
    }
}
