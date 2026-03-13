use anyhow::Result;

/// 入口：加载配置 → 初始化日志 → 启动 LinkService（含内嵌 MCP Server）
#[tokio::main]
async fn main() -> Result<()> {
    let config = acp_link::config::AppConfig::discover()?;

    let log_dir = dirs::home_dir()
        .unwrap_or_else(|| std::path::PathBuf::from("."))
        .join(".acp-link")
        .join("logs");
    std::fs::create_dir_all(&log_dir)?;

    cleanup_old_logs(&log_dir, config.log_keep_days);

    let file_appender = tracing_appender::rolling::daily(&log_dir, "acp-link.log");
    let (non_blocking, _guard) = tracing_appender::non_blocking(file_appender);

    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::new(&config.log_level))
        .with_ansi(false)
        .with_writer(non_blocking)
        .init();

    tracing::info!(
        "data_dir: {}",
        acp_link::config::AppConfig::data_dir().display()
    );

    let service = acp_link::link::LinkService::new(&config).await?;
    service.run().await
}

/// 清理 log_dir 中超过 keep_days 天的日志文件
fn cleanup_old_logs(log_dir: &std::path::Path, keep_days: u32) {
    let cutoff =
        std::time::SystemTime::now() - std::time::Duration::from_secs(u64::from(keep_days) * 86400);

    let entries = match std::fs::read_dir(log_dir) {
        Ok(e) => e,
        Err(_) => return,
    };

    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        // 只清理形如 acp-link.log.YYYY-MM-DD 的滚动日志文件
        let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
        if !name.starts_with("acp-link.log.") {
            continue;
        }
        if let Ok(metadata) = std::fs::metadata(&path) {
            if let Ok(modified) = metadata.modified() {
                if modified < cutoff {
                    let _ = std::fs::remove_file(&path);
                }
            }
        }
    }
}
