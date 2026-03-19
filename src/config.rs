use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// 飞书应用配置
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FeishuConfig {
    pub app_id: String,
    pub app_secret: String,
}

/// Kiro CLI 配置
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KiroConfig {
    /// 可执行文件路径，例如 "kiro-cli"
    pub cmd: String,
    /// 启动参数，例如 ["acp"]
    #[serde(default)]
    pub args: Vec<String>,
    /// kiro-cli 进程池大小，通过 thread_id hash 路由实现并行处理，默认为 4
    #[serde(default = "default_pool_size")]
    pub pool_size: usize,
    /// Agent 工作目录，用作项目上下文路径；未配置时默认为 `~/.acp-link/temp/`
    pub cwd: Option<PathBuf>,
}

fn default_pool_size() -> usize {
    4
}

impl KiroConfig {
    /// 返回有效的工作目录：优先使用配置的 `cwd`，否则回退到 `~/.acp-link/temp/`
    pub fn effective_cwd(&self) -> PathBuf {
        self.cwd.clone().unwrap_or_else(AppConfig::temp_dir)
    }
}

fn default_im_platform() -> String {
    "feishu".to_string()
}

fn default_log_level() -> String {
    "info".to_string()
}

fn default_log_retention() -> u32 {
    7
}

fn default_session_retention() -> u32 {
    7
}

fn default_resource_retention() -> u32 {
    7
}

/// 返回 `~/.acp-link/` 目录路径
fn default_home_dir() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".acp-link")
}

/// MCP Server 配置
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpConfig {
    /// HTTP 监听端口，默认 9800
    #[serde(default = "default_mcp_port")]
    pub port: u16,
}

impl Default for McpConfig {
    fn default() -> Self {
        Self {
            port: default_mcp_port(),
        }
    }
}

fn default_mcp_port() -> u16 {
    9800
}

/// 应用全局配置
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppConfig {
    /// 当前使用的 IM 平台（"feishu"）
    #[serde(default = "default_im_platform")]
    pub im_platform: String,
    /// 日志级别，例如 "info", "debug", "warn"
    #[serde(default = "default_log_level")]
    pub log_level: String,
    /// 日志保留天数，默认 7 天
    #[serde(default = "default_log_retention")]
    pub log_retention: u32,
    /// Session 保留天数，默认 3 天
    #[serde(default = "default_session_retention")]
    pub session_retention: u32,
    /// 资源文件保留天数，默认 3 天
    #[serde(default = "default_resource_retention")]
    pub resource_retention: u32,
    pub feishu: FeishuConfig,
    pub kiro: KiroConfig,
    /// MCP Server 配置（可选，使用默认值）
    #[serde(default)]
    pub mcp: McpConfig,
}

impl AppConfig {
    /// 数据目录：`~/.acp-link/data/`
    pub fn data_dir() -> PathBuf {
        default_home_dir().join("data")
    }

    /// 日志目录：`~/.acp-link/logs/`
    pub fn log_dir() -> PathBuf {
        default_home_dir().join("logs")
    }

    /// 临时目录：`~/.acp-link/temp/`（用作 kiro-cli 工作目录）
    pub fn temp_dir() -> PathBuf {
        default_home_dir().join("temp")
    }

    /// 创建默认配置文件模板
    fn create_default(path: &Path) -> Result<()> {
        const DEFAULT_CONFIG: &str = r#"log_level = "info"

[feishu]
app_id = "YOUR_APP_ID"
app_secret = "YOUR_APP_SECRET"

[kiro]
cmd = "kiro-cli"
args = ["acp", "--agent", "lark"]
"#;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("创建配置目录失败: {}", parent.display()))?;
        }
        std::fs::write(path, DEFAULT_CONFIG)
            .with_context(|| format!("写入默认配置文件失败: {}", path.display()))?;
        Ok(())
    }

    /// 从指定路径加载配置文件（TOML 格式）
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use acp_link::config::AppConfig;
    /// let config = AppConfig::load("config.toml").unwrap();
    /// ```
    pub fn load(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        let content = std::fs::read_to_string(path)
            .with_context(|| format!("读取配置文件失败: {}", path.display()))?;
        let config: Self = toml::from_str(&content)
            .with_context(|| format!("解析配置文件失败: {}", path.display()))?;
        const SUPPORTED_PLATFORMS: &[&str] = &["feishu"];
        if !SUPPORTED_PLATFORMS.contains(&config.im_platform.as_str()) {
            anyhow::bail!(
                "不支持的 IM 平台: \"{}\"，当前支持: {:?}",
                config.im_platform,
                SUPPORTED_PLATFORMS
            );
        }
        config.ensure_dirs()?;
        Ok(config)
    }

    /// 确保数据目录存在，不存在则自动创建
    fn ensure_dirs(&self) -> Result<()> {
        let data_dir = Self::data_dir();
        std::fs::create_dir_all(&data_dir)
            .with_context(|| format!("创建数据目录失败: {}", data_dir.display()))?;
        let temp_dir = Self::temp_dir();
        std::fs::create_dir_all(&temp_dir)
            .with_context(|| format!("创建临时目录失败: {}", temp_dir.display()))?;
        Ok(())
    }

    /// 按优先级查找并加载配置文件：
    /// 1. 环境变量 `ACP_LINK_CONFIG` 指定的路径
    /// 2. 当前目录下的 `config.toml`
    /// 3. `~/.acp-link/config.toml`
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use acp_link::config::AppConfig;
    /// let config = AppConfig::discover().unwrap();
    /// ```
    pub fn discover() -> Result<Self> {
        if let Ok(env_path) = std::env::var("ACP_LINK_CONFIG") {
            return Self::load(&env_path);
        }

        let local_path = PathBuf::from("config.toml");
        if local_path.exists() {
            return Self::load(&local_path);
        }

        let global_path = default_home_dir().join("config.toml");
        if global_path.exists() {
            return Self::load(&global_path);
        }

        // 未找到配置文件，创建默认配置并提醒用户修改
        Self::create_default(&global_path)?;
        anyhow::bail!(
            "已创建默认配置文件: {}\n请修改其中的飞书 app_id/app_secret 等参数后重新启动",
            global_path.display()
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 构造最小合法配置 TOML 内容
    fn minimal_config_toml() -> &'static str {
        r#"
[feishu]
app_id = "cli_test"
app_secret = "secret123"

[kiro]
cmd = "kiro"
args = ["acp"]
"#
    }

    fn make_temp_dir() -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "acp_link_test_{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).expect("创建临时目录失败");
        dir
    }

    #[test]
    fn test_load_valid_config() {
        let tmp = make_temp_dir();
        let config_path = tmp.join("config.toml");
        std::fs::write(&config_path, minimal_config_toml()).unwrap();

        let config = AppConfig::load(&config_path).expect("加载配置应成功");
        assert_eq!(config.feishu.app_id, "cli_test");
        assert_eq!(config.feishu.app_secret, "secret123");
        assert_eq!(config.kiro.cmd, "kiro");
        assert_eq!(config.kiro.args, vec!["acp"]);

        std::fs::remove_dir_all(&tmp).ok();
    }

    #[test]
    fn test_load_default_log_level() {
        let tmp = make_temp_dir();
        let config_path = tmp.join("config.toml");
        std::fs::write(&config_path, minimal_config_toml()).unwrap();

        let config = AppConfig::load(&config_path).unwrap();
        assert_eq!(config.log_level, "info");

        std::fs::remove_dir_all(&tmp).ok();
    }

    #[test]
    fn test_load_custom_log_level() {
        let tmp = make_temp_dir();
        let content = format!("log_level = \"debug\"\n{}", minimal_config_toml());
        let config_path = tmp.join("config.toml");
        std::fs::write(&config_path, &content).unwrap();

        let config = AppConfig::load(&config_path).unwrap();
        assert_eq!(config.log_level, "debug");

        std::fs::remove_dir_all(&tmp).ok();
    }

    #[test]
    fn test_load_default_pool_size() {
        let tmp = make_temp_dir();
        let config_path = tmp.join("config.toml");
        std::fs::write(&config_path, minimal_config_toml()).unwrap();

        let config = AppConfig::load(&config_path).unwrap();
        assert_eq!(config.kiro.pool_size, 4);

        std::fs::remove_dir_all(&tmp).ok();
    }

    #[test]
    fn test_load_custom_pool_size() {
        let tmp = make_temp_dir();
        let content = r#"
[feishu]
app_id = "x"
app_secret = "y"

[kiro]
cmd = "kiro"
pool_size = 8
"#;
        let config_path = tmp.join("config.toml");
        std::fs::write(&config_path, content).unwrap();

        let config = AppConfig::load(&config_path).unwrap();
        assert_eq!(config.kiro.pool_size, 8);

        std::fs::remove_dir_all(&tmp).ok();
    }

    #[test]
    fn test_load_nonexistent_file_returns_error() {
        let result = AppConfig::load("/nonexistent/path/config.toml");
        assert!(result.is_err());
    }

    #[test]
    fn test_load_invalid_toml_returns_error() {
        let tmp = make_temp_dir();
        let config_path = tmp.join("config.toml");
        std::fs::write(&config_path, "not valid toml :::").unwrap();

        let result = AppConfig::load(&config_path);
        assert!(result.is_err());

        std::fs::remove_dir_all(&tmp).ok();
    }

    #[test]
    fn test_discover_via_env_var() {
        let tmp = make_temp_dir();
        let config_path = tmp.join("config.toml");
        std::fs::write(&config_path, minimal_config_toml()).unwrap();

        // SAFETY: 单线程测试环境，set_var/remove_var 不存在数据竞争
        unsafe {
            std::env::set_var("ACP_LINK_CONFIG", config_path.to_str().unwrap());
        }
        let result = AppConfig::discover();
        unsafe {
            std::env::remove_var("ACP_LINK_CONFIG");
        }

        assert!(result.is_ok());
        assert_eq!(result.unwrap().feishu.app_id, "cli_test");

        std::fs::remove_dir_all(&tmp).ok();
    }

    #[test]
    fn test_data_dir_contains_acp_link() {
        let data_dir = AppConfig::data_dir();
        assert!(data_dir.to_string_lossy().contains(".acp-link"));
        assert_eq!(data_dir.file_name().and_then(|s| s.to_str()), Some("data"));
    }
}
