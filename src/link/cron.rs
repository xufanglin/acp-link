//! 定时任务调度模块
//!
//! 根据配置的 cron 表达式，定期向 ACP agent 发送 prompt，
//! 并将 agent 的流式响应以消息卡片形式发送到指定的 IM 会话。
//!
//! 通过 `watch::Receiver<Vec<CronJob>>` 支持热重载：
//! 收到新配置时关闭旧 scheduler，重建并启动新的。

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Result;
use tokio::sync::watch;
use tokio::time::{Duration, Instant};
use tokio_cron_scheduler::{Job, JobScheduler};

use super::acp::{AcpBridge, StreamEvent};
use crate::config::CronJob;
use crate::im::IMChannel;

/// 消息流式更新的节流间隔（与主消息处理保持一致）
const CRON_UPDATE_INTERVAL: Duration = Duration::from_millis(300);

/// 启动 cron 调度器，监听配置变化后自动热重载
///
/// `cron_rx` 初始值即为启动时的 job 列表，后续通过 `watch::Sender` 推送新列表实现热重载。
pub async fn start(
    mut cron_rx: watch::Receiver<Vec<CronJob>>,
    bridge: AcpBridge,
    channel: Arc<dyn IMChannel>,
    cwd: PathBuf,
) -> Result<()> {
    loop {
        let jobs = cron_rx.borrow_and_update().clone();

        // 空列表时不创建 scheduler，直接等待下次更新
        let scheduler = if jobs.is_empty() {
            tracing::info!("cron: 无定时任务配置，等待热重载...");
            None
        } else {
            Some(build_scheduler(jobs, &bridge, &channel, &cwd).await?)
        };

        // 等待配置变化
        cron_rx.changed().await.ok();

        // 关闭旧 scheduler
        if let Some(mut s) = scheduler {
            if let Err(e) = s.shutdown().await {
                tracing::warn!("cron: 关闭旧 scheduler 失败: {e}");
            }
        }

        tracing::info!("cron: 收到新配置，重建 scheduler");
    }
}

/// 根据 job 列表构建并启动 JobScheduler
async fn build_scheduler(
    jobs: Vec<CronJob>,
    bridge: &AcpBridge,
    channel: &Arc<dyn IMChannel>,
    cwd: &PathBuf,
) -> Result<JobScheduler> {
    let scheduler = JobScheduler::new().await?;

    for job_cfg in jobs {
        let bridge = bridge.clone();
        let channel = channel.clone();
        let cwd = cwd.clone();

        // tokio-cron-scheduler 使用 quartz 格式（6位，带秒），
        // 标准5位 cron 需要前面加 "0 " 以兼容（秒=0）
        let schedule = if job_cfg.schedule.split_whitespace().count() == 5 {
            format!("0 {}", job_cfg.schedule)
        } else {
            job_cfg.schedule.clone()
        };

        let job_cfg_for_closure = job_cfg.clone();
        let job = Job::new_async(schedule.as_str(), move |_uuid, _lock| {
            let bridge = bridge.clone();
            let channel = channel.clone();
            let cwd = cwd.clone();
            let job_cfg = job_cfg_for_closure.clone();

            Box::pin(async move {
                tracing::info!(
                    target_id = %job_cfg.target_id,
                    prompt = %job_cfg.prompt,
                    "cron job 触发"
                );

                if let Err(e) = run_cron_job(&bridge, &channel, &job_cfg, &cwd).await {
                    tracing::error!(
                        target_id = %job_cfg.target_id,
                        "cron job 执行失败: {e}"
                    );
                }
            })
        })?;

        scheduler.add(job).await?;
        tracing::info!(
            schedule = %job_cfg.schedule,
            target_id = %job_cfg.target_id,
            "已注册 cron job"
        );
    }

    scheduler.start().await?;
    Ok(scheduler)
}

/// 执行单次 cron 任务：创建 session → 发送 prompt → 流式更新消息
async fn run_cron_job(
    bridge: &AcpBridge,
    channel: &Arc<dyn IMChannel>,
    job: &CronJob,
    cwd: &PathBuf,
) -> Result<()> {
    // 使用 target_id 作为 routing_key，保证同一会话路由到同一 worker
    let routing_key = &job.target_id;

    // 每次 cron 触发都创建全新 session（独立上下文）
    let session_id = bridge.new_session(routing_key, cwd.clone()).await?;

    // 先发一条占位消息
    let message_id = channel
        .send_card(&job.target_id, &job.target_type, "⏳ 处理中...")
        .await?;

    // 发送 prompt 并获取流式 receiver
    let blocks = vec![AcpBridge::text_block(&job.prompt)];
    let mut rx = bridge
        .prompt_stream(routing_key, &session_id, blocks)
        .await?;

    // 流式消费并节流更新消息
    let mut content = String::new();
    let mut last_update = Instant::now();

    while let Some(event) = rx.recv().await {
        let in_tool_call = matches!(&event, StreamEvent::ToolCall(_));
        match event {
            StreamEvent::Text(chunk) => {
                content.push_str(&chunk);
            }
            StreamEvent::ToolCall(_) => {}
        }

        if last_update.elapsed() >= CRON_UPDATE_INTERVAL && !content.is_empty() {
            let snapshot = if in_tool_call {
                format!("{}\n...", content.trim_start_matches('\n'))
            } else {
                content.trim_start_matches('\n').to_string()
            };
            if let Err(e) = channel.update_message(&message_id, &snapshot).await {
                tracing::warn!("cron job 更新消息失败: {e}");
            }
            last_update = Instant::now();
        }
    }

    // 最终更新
    if !content.is_empty() {
        if let Err(e) = channel.update_message(&message_id, &content).await {
            tracing::warn!("cron job 最终更新消息失败: {e}");
        }
    }

    Ok(())
}
