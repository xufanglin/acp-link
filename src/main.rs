use std::sync::Arc;

use anyhow::Result;

/// 入口：加载配置 → 初始化滚动日志 → 根据 IM 配置创建 channel → 启动 LinkService
///
/// LinkService 内部会启动：
/// - IM 消息监听循环（WS 长连接，断开自动重连）
/// - ACP 进程池（agent 子进程，`!Send` 隔离）
/// - 内嵌 MCP HTTP Server（供 agent 反向调用 IM 能力）
/// - 定时清理任务（session / 资源 / 日志）
/// - 优雅关机处理（SIGTERM / Ctrl+C）
#[tokio::main]
async fn main() -> Result<()> {
    println!("acp-link v0.2.7");

    let config_path = acp_link::config::AppConfig::find_config_path()?;
    let config = acp_link::config::AppConfig::load(&config_path)?;

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

    let channel: Arc<dyn acp_link::im::IMChannel> = if let Some(ref feishu) = config.im.feishu {
        Arc::new(acp_link::im::FeishuChannel::new(
            &feishu.app_id,
            &feishu.app_secret,
        ))
    } else {
        anyhow::bail!("未配置 IM 平台，请在 [im.feishu] 中填写配置")
    };

    let service = acp_link::link::LinkService::new(&config, config_path, channel).await?;
    service.run().await
}
