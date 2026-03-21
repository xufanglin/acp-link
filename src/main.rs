use std::sync::Arc;

use anyhow::Result;

/// 入口：加载配置 → 初始化日志 → 启动 LinkService（含内嵌 MCP Server）
#[tokio::main]
async fn main() -> Result<()> {
    println!("acp-link v0.2.4");

    let config = acp_link::config::AppConfig::discover()?;

    let log_dir = acp_link::config::AppConfig::log_dir();
    std::fs::create_dir_all(&log_dir)?;

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

    let channel: Arc<dyn acp_link::im::IMChannel> = match config.im_platform.as_str() {
        "feishu" => Arc::new(acp_link::im::FeishuChannel::new(
            &config.feishu.app_id,
            &config.feishu.app_secret,
        )),
        _ => anyhow::bail!("不支持的 IM 平台: {}", config.im_platform),
    };

    let service = acp_link::link::LinkService::new(&config, channel).await?;
    service.run().await
}
