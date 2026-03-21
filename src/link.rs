//! 核心服务模块
//!
//! [`LinkService`] 是 acp-link 的主服务，负责：
//!
//! - 监听 IM 消息（通过 `IMChannel::listen()`，WS 断开自动重连）
//! - 每条消息 spawn 独立 task 并发处理
//! - 管理 session 生命周期（全量模式 / 增量模式）
//! - 构建 ACP prompt content blocks（文本 / 图片 / 文件）
//! - 流式消费 agent 响应并节流更新 IM 消息（300ms 间隔）
//! - 定时清理过期 session、资源文件、临时目录和历史日志
//! - 优雅关机（SIGTERM / Ctrl+C → flush session 映射）
//!
//! ## 子模块
//!
//! - [`acp`] — ACP 桥接（kiro-cli 进程池、`!Send` 隔离、流式 chunk 转发）
//! - [`session`] — Session 映射持久化（topic_id ↔ session_id）
//! - [`resource`] — 资源文件存储（SHA256 去重、过期清理）

mod acp;
mod resource;
mod session;

use std::path::PathBuf;
use std::sync::Arc;

use agent_client_protocol::ContentBlock;
use anyhow::Result;
use tokio::sync::RwLock;
use tokio::time::{Duration, Instant};

use std::collections::HashSet;

use self::acp::{AcpBridge, StreamEvent};
use self::resource::ResourceStore;
use self::session::SessionMap;

use crate::config::AppConfig;
use crate::im::{IMChannel, ImMessage, ImMessageContent};

/// 消息流式更新的节流间隔
const MESSAGE_UPDATE_INTERVAL: Duration = Duration::from_millis(300);

/// Link 服务的共享状态，多个消息处理任务并发访问
struct SharedState {
    /// IM channel（平台无关）
    channel: Arc<dyn IMChannel>,
    /// ACP 桥接（kiro-cli 进程池）
    bridge: AcpBridge,
    /// 资源文件存储（SHA256 去重）
    resource_store: ResourceStore,
    /// thread_id ↔ session_id 映射（持久化）
    session_map: RwLock<SessionMap>,
    /// 已在 ACP worker 中加载的 session_id 集合（跳过重复 load_session）
    loaded_sessions: RwLock<HashSet<String>>,
    /// 工作目录，传递给 ACP session
    cwd: PathBuf,
    /// Session 保留天数
    session_retention: u32,
    /// 资源文件保留天数
    resource_retention: u32,
    /// 日志保留天数
    log_retention: u32,
    /// 日志目录
    log_dir: PathBuf,
}

/// Link 服务：IM 消息监听 → ACP 转发 → 回复
///
/// 内部通过 `Arc<SharedState>` 共享状态，支持多条消息并发处理。
pub struct LinkService {
    state: Arc<SharedState>,
}

impl LinkService {
    /// 从配置初始化所有组件
    pub async fn new(config: &AppConfig, channel: Arc<dyn IMChannel>) -> Result<Self> {
        let data_dir = AppConfig::data_dir();
        let resource_store = ResourceStore::new(&data_dir);
        let sessions_path = data_dir.parent().unwrap_or(&data_dir).join("sessions.json");
        let session_map = SessionMap::load(&sessions_path)?;
        let bridge = AcpBridge::start(&config.kiro).await?;
        let cwd = config.kiro.effective_cwd();

        // 启动内嵌 MCP HTTP Server
        let mcp_channel = channel.clone();
        let mcp_port = config.mcp.port;
        tokio::spawn(async move {
            if let Err(e) = crate::mcp::start_mcp_server(mcp_channel, mcp_port).await {
                tracing::error!("MCP Server 异常退出: {e}");
            }
        });

        Ok(Self {
            state: Arc::new(SharedState {
                channel,
                bridge,
                resource_store,
                session_map: RwLock::new(session_map),
                loaded_sessions: RwLock::new(HashSet::new()),
                cwd,
                session_retention: config.session_retention,
                resource_retention: config.resource_retention,
                log_retention: config.log_retention,
                log_dir: AppConfig::log_dir(),
            }),
        })
    }

    /// 运行主循环：监听 IM 消息 + 定期清理 session + 优雅关机
    pub async fn run(&self) -> Result<()> {
        let (tx, mut rx) = tokio::sync::mpsc::channel(32);
        let channel_clone = self.state.channel.clone();
        tokio::spawn(async move {
            loop {
                if let Err(e) = channel_clone.listen(tx.clone()).await {
                    tracing::error!("WS error: {e}, reconnecting...");
                    tokio::time::sleep(Duration::from_secs(5)).await;
                }
            }
        });

        self.run_cleanup().await;

        let mut cleanup_interval = tokio::time::interval(Duration::from_secs(3600));
        cleanup_interval.tick().await;

        let shutdown = shutdown_signal();
        tokio::pin!(shutdown);

        loop {
            tokio::select! {
                biased;

                _ = cleanup_interval.tick() => {
                    self.run_cleanup().await;
                }

                msg = rx.recv() => {
                    let Some(msg) = msg else { break };
                    // 每条消息 spawn 独立任务，实现并发处理
                    let state = Arc::clone(&self.state);
                    tokio::spawn(async move {
                        handle_message(state, msg).await;
                    });
                }

                _ = &mut shutdown => {
                    tracing::info!("收到关机信号，正在优雅退出...");
                    if let Err(e) = self.state.session_map.read().await.flush() {
                        tracing::error!("关机时 session flush 失败: {e}");
                    }
                    break;
                }
            }
        }
        Ok(())
    }

    /// 清理过期的 session 映射、资源文件和临时目录
    async fn run_cleanup(&self) {
        if let Err(e) = self
            .state
            .session_map
            .write()
            .await
            .cleanup_expired(self.state.session_retention)
        {
            tracing::error!("Session 清理失败: {e}");
        }
        if let Err(e) = self
            .state
            .resource_store
            .cleanup_expired(self.state.resource_retention)
        {
            tracing::error!("资源清理失败: {e}");
        }
        if let Err(e) = cleanup_temp_dir(self.state.resource_retention) {
            tracing::error!("临时目录清理失败: {e}");
        }
        cleanup_old_logs(&self.state.log_dir, self.state.log_retention);
    }
}

/// 清理 log_dir 中超过 retention 天的日志文件
fn cleanup_old_logs(log_dir: &std::path::Path, retention: u32) {
    let cutoff =
        std::time::SystemTime::now() - std::time::Duration::from_secs(u64::from(retention) * 86400);

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

/// 清理临时目录中过期的文件
fn cleanup_temp_dir(retention: u32) -> Result<usize> {
    let temp_dir = AppConfig::temp_dir();
    let dir = match std::fs::read_dir(&temp_dir) {
        Ok(d) => d,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(e) => {
            return Err(
                anyhow::anyhow!(e).context(format!("读取临时目录失败: {}", temp_dir.display()))
            );
        }
    };

    let now = std::time::SystemTime::now();
    let ttl = Duration::from_secs(u64::from(retention) * 24 * 3600);
    let mut removed = 0;

    for entry in dir.flatten() {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let modified = match entry.metadata().and_then(|m| m.modified()) {
            Ok(t) => t,
            Err(_) => continue,
        };
        if now.duration_since(modified).unwrap_or_default() > ttl {
            if let Err(e) = std::fs::remove_file(&path) {
                tracing::warn!("删除过期临时文件失败: {} - {e}", path.display());
            } else {
                removed += 1;
            }
        }
    }

    if removed > 0 {
        tracing::info!("临时目录清理完成: 删除 {removed} 个过期文件");
    }
    Ok(removed)
}

/// 处理单条 IM 消息：根据是否有 topic 上下文决定新建会话或增量追加
async fn handle_message(state: Arc<SharedState>, msg: ImMessage) {
    tracing::info!(
        "[{}] {} message_id: {}",
        msg.chat_id,
        format_summary(&msg.content),
        msg.message_id
    );

    let is_actionable = is_actionable_message(&msg);

    match &msg.topic_id {
        None => {
            let hint = if is_actionable {
                "..."
            } else {
                "收到，请继续输入指令"
            };
            match state.channel.reply_message(&msg.message_id, hint).await {
                Ok((reply_msg_id, thread_id)) => {
                    if let Err(e) = state
                        .session_map
                        .write()
                        .await
                        .map_topic(&msg.message_id, &thread_id)
                    {
                        tracing::error!("持久化 thread 映射失败: {e}");
                    }
                    if is_actionable {
                        stream_acp_reply(&state, &thread_id, &msg.chat_id, &reply_msg_id, &msg)
                            .await;
                    }
                }
                Err(e) => tracing::error!("创建消息话题失败: {e}"),
            }
        }
        Some(root_id) => {
            let thread_id = state
                .session_map
                .read()
                .await
                .get_topic_id(root_id)
                .map(str::to_owned);
            match thread_id {
                Some(thread_id) => {
                    if is_actionable {
                        submit_to_acp_streaming(
                            &state,
                            &thread_id,
                            &msg.chat_id,
                            &msg.message_id,
                            &msg,
                        )
                        .await;
                    } else {
                        if let Err(e) = state
                            .channel
                            .reply_message(&msg.message_id, "收到附件，请回复文字指令来处理它")
                            .await
                        {
                            tracing::error!("回复附件提示失败: {e}");
                        }
                    }
                }
                None => {
                    tracing::debug!("[{}] root_id={root_id} 无映射，作为新会话处理", msg.chat_id);
                    let hint = if is_actionable {
                        "..."
                    } else {
                        "收到，请继续输入指令"
                    };
                    match state.channel.reply_message(&msg.message_id, hint).await {
                        Ok((reply_msg_id, thread_id)) => {
                            if let Err(e) = state
                                .session_map
                                .write()
                                .await
                                .map_topic(root_id, &thread_id)
                            {
                                tracing::error!("持久化 thread 映射失败: {e}");
                            }
                            if is_actionable {
                                stream_acp_reply(
                                    &state,
                                    &thread_id,
                                    &msg.chat_id,
                                    &reply_msg_id,
                                    &msg,
                                )
                                .await;
                            }
                        }
                        Err(e) => tracing::error!("创建消息话题失败: {e}"),
                    }
                }
            }
        }
    }
}

/// 已有 thread 内的文本消息：reply_message 和 prepare_prompt 并行，然后流式更新
async fn submit_to_acp_streaming(
    state: &Arc<SharedState>,
    thread_id: &str,
    chat_id: &str,
    reply_to_message_id: &str,
    msg: &ImMessage,
) {
    let reply_fut = state.channel.reply_message(reply_to_message_id, "...");
    let prompt_fut = prepare_prompt(state, thread_id, chat_id, msg);
    let (reply_result, prompt_result) = tokio::join!(reply_fut, prompt_fut);

    let reply_msg_id = match reply_result {
        Ok((reply_msg_id, _)) => reply_msg_id,
        Err(e) => {
            tracing::error!("创建回复消息失败: {e}");
            return;
        }
    };

    match prompt_result {
        Ok((session_id, blocks)) => {
            stream_acp_reply_prepared(state, thread_id, &session_id, &reply_msg_id, blocks).await;
        }
        Err(e) => {
            tracing::error!("准备 prompt 失败: {e}");
            let _ = state
                .channel
                .update_message(&reply_msg_id, &format!("处理失败: {e}"))
                .await;
        }
    }
}

/// 构建 blocks → 发送 ACP prompt → 流式更新回复消息
async fn stream_acp_reply(
    state: &Arc<SharedState>,
    thread_id: &str,
    chat_id: &str,
    reply_message_id: &str,
    msg: &ImMessage,
) {
    let prompt_result = prepare_prompt(state, thread_id, chat_id, msg).await;
    match prompt_result {
        Ok((session_id, blocks)) => {
            stream_acp_reply_prepared(state, thread_id, &session_id, reply_message_id, blocks)
                .await;
        }
        Err(e) => {
            tracing::error!("准备 prompt 失败: {e}");
            let _ = state
                .channel
                .update_message(reply_message_id, &format!("处理失败: {e}"))
                .await;
        }
    }
}

/// 已准备好 prompt 的流式处理：发送到 ACP → 接收 chunk 并节流更新回复消息
async fn stream_acp_reply_prepared(
    state: &Arc<SharedState>,
    routing_key: &str,
    session_id: &str,
    reply_message_id: &str,
    blocks: Vec<ContentBlock>,
) {
    match do_stream_prepared(state, routing_key, session_id, reply_message_id, blocks).await {
        Ok(()) => {}
        Err(e) => {
            tracing::error!("流式处理失败: {e}");
            let _ = state
                .channel
                .update_message(reply_message_id, &format!("处理失败: {e}"))
                .await;
        }
    }
}

/// 构建 content blocks 并获取/创建 session
async fn prepare_prompt(
    state: &Arc<SharedState>,
    thread_id: &str,
    chat_id: &str,
    msg: &ImMessage,
) -> Result<(String, Vec<ContentBlock>)> {
    // 先读锁查 session_id
    let existing_sid = state
        .session_map
        .read()
        .await
        .get_session_id(thread_id)
        .map(str::to_owned);

    match existing_sid {
        Some(sid) => {
            // 已有 session：增量，只发当前消息文本
            tracing::debug!("增量 prompt: thread={thread_id} -> session={sid}");
            let already_loaded = state.loaded_sessions.read().await.contains(&sid);
            if already_loaded {
                tracing::debug!("session 已缓存，跳过 load_session: {sid}");
            } else {
                state
                    .bridge
                    .load_session(thread_id, &sid, state.cwd.clone())
                    .await?;
                state.loaded_sessions.write().await.insert(sid.clone());
            }
            let session_id = sid;
            let text = match &msg.content {
                ImMessageContent::Text(t) => t.clone(),
                _ => anyhow::bail!("增量模式仅支持文本消息"),
            };
            let context = format!(
                "[im_context: message_id={}, chat_id={}]\n\n{}",
                msg.message_id, msg.chat_id, text
            );
            Ok((session_id, vec![AcpBridge::text_block(&context)]))
        }
        None => {
            // 新 session：全量聚合 thread 内容
            tracing::debug!("全量聚合: thread={thread_id}");
            let submission = state.channel.aggregate_topic(thread_id, chat_id).await?;
            tracing::info!(
                "聚合完成: topic={}, texts={}, images={}, files={}",
                submission.topic_id,
                submission.texts.len(),
                submission.images.len(),
                submission.files.len(),
            );

            let mut blocks: Vec<ContentBlock> = Vec::new();

            for text in &submission.texts {
                blocks.push(AcpBridge::text_block(text));
            }

            for img in &submission.images {
                let data = state
                    .channel
                    .download_resource(&img.message_id, &img.image_key, "image")
                    .await?;
                let mime = detect_image_mime(&data);
                tracing::debug!(
                    "图片已下载: {} ({} bytes, {})",
                    img.image_key,
                    data.len(),
                    mime
                );
                blocks.push(AcpBridge::image_block(&data, mime));
            }

            for file in &submission.files {
                let path = state
                    .resource_store
                    .save_resource(
                        state.channel.as_ref(),
                        &file.message_id,
                        &file.file_key,
                        "file",
                        &file.file_name,
                    )
                    .await?;
                let uri = ResourceStore::to_file_uri(&path);
                let mime = mime_from_filename(&file.file_name);
                blocks.push(AcpBridge::resource_link_block(&file.file_name, &uri, mime));
            }

            // 在最前面注入 im_context，供 agent 提取 message_id
            blocks.insert(
                0,
                AcpBridge::text_block(&format!(
                    "[im_context: message_id={}, chat_id={}]",
                    msg.message_id, msg.chat_id
                )),
            );

            if blocks.len() <= 1 {
                anyhow::bail!("无有效内容可提交");
            }

            let sid = state
                .bridge
                .new_session(thread_id, state.cwd.clone())
                .await?;
            // 写锁插入 session 映射
            state.session_map.write().await.insert(thread_id, &sid)?;
            state.loaded_sessions.write().await.insert(sid.clone());
            tracing::debug!("新建 session: thread={thread_id} -> session={sid}");
            Ok((sid, blocks))
        }
    }
}

/// 核心流式处理：发送到 ACP → 接收 chunk 并节流更新回复消息（prompt 已准备好）
async fn do_stream_prepared(
    state: &Arc<SharedState>,
    routing_key: &str,
    session_id: &str,
    reply_message_id: &str,
    blocks: Vec<ContentBlock>,
) -> Result<()> {
    for (i, block) in blocks.iter().enumerate() {
        match block {
            ContentBlock::Text(t) => {
                tracing::debug!("  block[{i}]: Text({} chars)", t.text.len())
            }
            ContentBlock::Image(img) => tracing::debug!(
                "  block[{i}]: Image(mime={}, data={} chars)",
                img.mime_type,
                img.data.len()
            ),
            ContentBlock::ResourceLink(r) => {
                tracing::debug!("  block[{i}]: ResourceLink(uri={})", r.uri)
            }
            _ => tracing::debug!("  block[{i}]: {:?}", std::mem::discriminant(block)),
        }
    }
    tracing::info!(
        "发送 ACP prompt: session={session_id}, blocks={}",
        blocks.len()
    );
    let mut chunk_rx = state
        .bridge
        .prompt_stream(routing_key, session_id, blocks)
        .await?;
    let stream_start = Instant::now();
    tracing::debug!("ACP prompt 流开始: session={session_id}");

    let mut full_text = String::new();
    // 初始时间设为"很久以前"，确保首个 chunk 立即触发消息更新
    let mut last_update = Instant::now() - MESSAGE_UPDATE_INTERVAL;
    let mut dirty = false;
    let mut chunk_count: u64 = 0;
    let mut first_chunk_logged = false;
    // 跟踪后台消息更新任务，避免并发更新冲突
    let mut inflight: Option<tokio::task::JoinHandle<()>> = None;
    while let Some(event) = chunk_rx.recv().await {
        chunk_count += 1;
        let in_tool_call = matches!(&event, StreamEvent::ToolCall(_));
        match event {
            StreamEvent::Text(chunk) => {
                if !first_chunk_logged {
                    tracing::info!(
                        "首个 chunk 到达: session={session_id}, 延迟={}ms",
                        stream_start.elapsed().as_millis()
                    );
                    first_chunk_logged = true;
                }
                full_text.push_str(&chunk);
                dirty = true;
            }
            StreamEvent::ToolCall(title) => {
                tracing::debug!("工具调用中: {title}, session={session_id}");
                dirty = true;
            }
        }

        if last_update.elapsed() >= MESSAGE_UPDATE_INTERVAL {
            // 如果上一次更新还在进行中，跳过本次（下次会带上所有累积 chunk）
            let should_send = match &inflight {
                Some(h) => h.is_finished(),
                None => true,
            };
            if should_send {
                let channel = state.channel.clone();
                let msg_id = reply_message_id.to_string();
                let trimmed = full_text.trim_start_matches('\n');
                let text_snapshot = if in_tool_call {
                    format!("{trimmed}\n...")
                } else {
                    trimmed.to_string()
                };
                inflight = Some(tokio::spawn(async move {
                    let t = Instant::now();
                    if let Err(e) = channel.update_message(&msg_id, &text_snapshot).await {
                        tracing::warn!("更新消息失败（将继续）: {e}");
                    }
                    tracing::debug!("消息更新耗时: {}ms", t.elapsed().as_millis());
                }));
                last_update = Instant::now();
                dirty = false;
            }
        }
    }

    // 等待最后一次后台更新完成
    if let Some(h) = inflight {
        if let Err(e) = h.await {
            tracing::error!("后台消息更新任务异常: {e}");
        }
    }

    if dirty || full_text.is_empty() {
        let final_text = if full_text.trim().is_empty() {
            "(无响应)".to_string()
        } else {
            full_text.trim_start_matches('\n').to_string()
        };
        if let Err(e) = state
            .channel
            .update_message(reply_message_id, &final_text)
            .await
        {
            tracing::error!("最终更新消息失败: {e}");
        }
    }

    tracing::info!(
        "ACP prompt 流结束: session={session_id}, chunks={chunk_count}, 总耗时={}ms",
        stream_start.elapsed().as_millis()
    );
    Ok(())
}

/// 等待关机信号（Ctrl+C 或 SIGTERM）
async fn shutdown_signal() {
    let ctrl_c = async {
        tokio::signal::ctrl_c()
            .await
            .expect("注册 Ctrl+C 信号处理器失败");
    };

    #[cfg(unix)]
    {
        let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("注册 SIGTERM 信号处理器失败");
        tokio::select! {
            _ = ctrl_c => {}
            _ = sigterm.recv() => {}
        }
    }

    #[cfg(not(unix))]
    ctrl_c.await;
}

/// 生成消息内容的简短摘要（用于日志）
fn format_summary(content: &ImMessageContent) -> String {
    match content {
        ImMessageContent::Text(t) => format!("文本: {t}"),
        ImMessageContent::Image { image_key } => format!("图片: {image_key}"),
        ImMessageContent::File {
            file_name,
            file_size,
            ..
        } => format!("文件: {file_name} ({file_size} bytes)"),
        ImMessageContent::Audio { duration_ms, .. } => format!("音频: {duration_ms}ms"),
        ImMessageContent::Media {
            file_name,
            duration_ms,
            ..
        } => format!("视频: {file_name} ({duration_ms}ms)"),
        ImMessageContent::Sticker { file_type, .. } => format!("表情: {file_type}"),
        ImMessageContent::Link { url } => format!("链接: {url}"),
        ImMessageContent::Unsupported { message_type, .. } => {
            format!("未支持类型: {message_type}")
        }
    }
}

/// 判断消息是否为文本类型
/// 判断消息是否为可执行指令（纯文本），Link 等素材类型不算
fn is_actionable_message(msg: &ImMessage) -> bool {
    matches!(&msg.content, ImMessageContent::Text(_))
}

/// 通过文件头魔数检测图片 MIME 类型
fn detect_image_mime(data: &[u8]) -> &'static str {
    if data.starts_with(&[0x89, 0x50, 0x4E, 0x47]) {
        "image/png"
    } else if data.starts_with(&[0xFF, 0xD8, 0xFF]) {
        "image/jpeg"
    } else if data.starts_with(b"GIF") {
        "image/gif"
    } else if data.starts_with(b"RIFF") && data.get(8..12) == Some(b"WEBP") {
        "image/webp"
    } else {
        "image/png" // fallback
    }
}

/// 根据文件扩展名推断 MIME 类型
fn mime_from_filename(name: &str) -> Option<&'static str> {
    let ext = name.rsplit('.').next()?.to_lowercase();
    Some(match ext.as_str() {
        "pdf" => "application/pdf",
        "doc" | "docx" => "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
        "xls" | "xlsx" => "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
        "ppt" | "pptx" => {
            "application/vnd.openxmlformats-officedocument.presentationml.presentation"
        }
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "txt" => "text/plain",
        "json" => "application/json",
        "md" => "text/markdown",
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::im::{ImMessage, ImMessageContent};

    /// 构造一条最简 ImMessage 用于测试
    fn make_msg(content: ImMessageContent) -> ImMessage {
        ImMessage {
            message_id: "msg_001".to_string(),
            chat_id: "chat_001".to_string(),
            chat_type: "p2p".to_string(),
            sender_id: "ou_xxx".to_string(),
            content,
            timestamp: 0,
            topic_id: None,
        }
    }

    // ── detect_image_mime ────────────────────────────────────────────────────

    #[test]
    fn test_detect_image_mime_png() {
        // PNG 魔数：\x89PNG
        let data = [0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A];
        assert_eq!(detect_image_mime(&data), "image/png");
    }

    #[test]
    fn test_detect_image_mime_jpeg() {
        // JPEG 魔数：\xFF\xD8\xFF
        let data = [0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x10];
        assert_eq!(detect_image_mime(&data), "image/jpeg");
    }

    #[test]
    fn test_detect_image_mime_gif() {
        // GIF 魔数：GIF
        let data = b"GIF89a\x01\x00";
        assert_eq!(detect_image_mime(data), "image/gif");
    }

    #[test]
    fn test_detect_image_mime_webp() {
        // WEBP 魔数：RIFF....WEBP
        let mut data = [0u8; 12];
        data[0..4].copy_from_slice(b"RIFF");
        data[4..8].copy_from_slice(&[0x00u8; 4]); // 文件大小（任意值）
        data[8..12].copy_from_slice(b"WEBP");
        assert_eq!(detect_image_mime(&data), "image/webp");
    }

    #[test]
    fn test_detect_image_mime_fallback() {
        // 未知格式应回退为 image/png
        let data = [0x00, 0x01, 0x02, 0x03];
        assert_eq!(detect_image_mime(&data), "image/png");
    }

    #[test]
    fn test_detect_image_mime_empty_data() {
        // 空数据应回退为 image/png
        assert_eq!(detect_image_mime(&[]), "image/png");
    }

    #[test]
    fn test_detect_image_mime_riff_without_webp() {
        // RIFF 头但不是 WEBP 应回退为 image/png
        let mut data = [0u8; 12];
        data[0..4].copy_from_slice(b"RIFF");
        data[8..12].copy_from_slice(b"WAVE");
        assert_eq!(detect_image_mime(&data), "image/png");
    }

    // ── mime_from_filename ───────────────────────────────────────────────────

    #[test]
    fn test_mime_from_filename_pdf() {
        assert_eq!(mime_from_filename("report.pdf"), Some("application/pdf"));
    }

    #[test]
    fn test_mime_from_filename_docx() {
        assert_eq!(
            mime_from_filename("doc.docx"),
            Some("application/vnd.openxmlformats-officedocument.wordprocessingml.document")
        );
    }

    #[test]
    fn test_mime_from_filename_doc() {
        // .doc 与 .docx 应映射相同 MIME
        assert_eq!(
            mime_from_filename("old.doc"),
            mime_from_filename("new.docx")
        );
    }

    #[test]
    fn test_mime_from_filename_xlsx() {
        assert_eq!(
            mime_from_filename("data.xlsx"),
            Some("application/vnd.openxmlformats-officedocument.spreadsheetml.sheet")
        );
    }

    #[test]
    fn test_mime_from_filename_pptx() {
        assert_eq!(
            mime_from_filename("slides.pptx"),
            Some("application/vnd.openxmlformats-officedocument.presentationml.presentation")
        );
    }

    #[test]
    fn test_mime_from_filename_png() {
        assert_eq!(mime_from_filename("image.png"), Some("image/png"));
    }

    #[test]
    fn test_mime_from_filename_jpeg() {
        assert_eq!(mime_from_filename("photo.jpg"), Some("image/jpeg"));
        assert_eq!(mime_from_filename("photo.jpeg"), Some("image/jpeg"));
    }

    #[test]
    fn test_mime_from_filename_txt() {
        assert_eq!(mime_from_filename("readme.txt"), Some("text/plain"));
    }

    #[test]
    fn test_mime_from_filename_json() {
        assert_eq!(mime_from_filename("data.json"), Some("application/json"));
    }

    #[test]
    fn test_mime_from_filename_md() {
        assert_eq!(mime_from_filename("README.md"), Some("text/markdown"));
    }

    #[test]
    fn test_mime_from_filename_unknown_extension() {
        // 未知扩展名应返回 None
        assert_eq!(mime_from_filename("archive.zip"), None);
        assert_eq!(mime_from_filename("binary.exe"), None);
    }

    #[test]
    fn test_mime_from_filename_no_extension() {
        // 无扩展名：rsplit('.') 返回文件名本身，不匹配任何分支
        assert_eq!(mime_from_filename("Makefile"), None);
    }

    #[test]
    fn test_mime_from_filename_uppercase_extension() {
        // 扩展名大写时应自动转小写匹配
        assert_eq!(mime_from_filename("IMAGE.PNG"), Some("image/png"));
        assert_eq!(mime_from_filename("DOC.PDF"), Some("application/pdf"));
    }

    #[test]
    fn test_mime_from_filename_multiple_dots() {
        // 多点文件名取最后一个扩展名
        assert_eq!(
            mime_from_filename("my.report.v2.pdf"),
            Some("application/pdf")
        );
    }

    // ── format_summary ───────────────────────────────────────────────────────

    #[test]
    fn test_format_summary_text() {
        let content = ImMessageContent::Text("你好世界".to_string());
        let summary = format_summary(&content);
        assert!(summary.contains("文本"));
        assert!(summary.contains("你好世界"));
    }

    #[test]
    fn test_format_summary_image() {
        let content = ImMessageContent::Image {
            image_key: "img_key_001".to_string(),
        };
        let summary = format_summary(&content);
        assert!(summary.contains("图片"));
        assert!(summary.contains("img_key_001"));
    }

    #[test]
    fn test_format_summary_file() {
        let content = ImMessageContent::File {
            file_key: "fk_001".to_string(),
            file_name: "report.pdf".to_string(),
            file_size: 2048,
        };
        let summary = format_summary(&content);
        assert!(summary.contains("文件"));
        assert!(summary.contains("report.pdf"));
        assert!(summary.contains("2048"));
    }

    #[test]
    fn test_format_summary_audio() {
        let content = ImMessageContent::Audio {
            file_key: "ak_001".to_string(),
            duration_ms: 5000,
        };
        let summary = format_summary(&content);
        assert!(summary.contains("音频"));
        assert!(summary.contains("5000"));
    }

    #[test]
    fn test_format_summary_media() {
        let content = ImMessageContent::Media {
            file_key: "vk_001".to_string(),
            file_name: "video.mp4".to_string(),
            duration_ms: 30000,
            width: 1920,
            height: 1080,
        };
        let summary = format_summary(&content);
        assert!(summary.contains("视频"));
        assert!(summary.contains("video.mp4"));
        assert!(summary.contains("30000"));
    }

    #[test]
    fn test_format_summary_sticker() {
        let content = ImMessageContent::Sticker {
            file_key: "stk_001".to_string(),
            file_type: "png".to_string(),
        };
        let summary = format_summary(&content);
        assert!(summary.contains("表情"));
        assert!(summary.contains("png"));
    }

    #[test]
    fn test_format_summary_unsupported() {
        let content = ImMessageContent::Unsupported {
            message_type: "location".to_string(),
            raw_content: "{}".to_string(),
        };
        let summary = format_summary(&content);
        assert!(summary.contains("未支持"));
        assert!(summary.contains("location"));
    }

    // ── is_actionable_message ──────────────────────────────────────────────

    #[test]
    fn test_is_actionable_message_true() {
        let msg = make_msg(ImMessageContent::Text("hello".to_string()));
        assert!(is_actionable_message(&msg));
    }

    #[test]
    fn test_is_actionable_message_false_for_image() {
        let msg = make_msg(ImMessageContent::Image {
            image_key: "k".to_string(),
        });
        assert!(!is_actionable_message(&msg));
    }

    #[test]
    fn test_is_actionable_message_false_for_file() {
        let msg = make_msg(ImMessageContent::File {
            file_key: "k".to_string(),
            file_name: "f.pdf".to_string(),
            file_size: 0,
        });
        assert!(!is_actionable_message(&msg));
    }

    #[test]
    fn test_is_actionable_message_false_for_audio() {
        let msg = make_msg(ImMessageContent::Audio {
            file_key: "k".to_string(),
            duration_ms: 0,
        });
        assert!(!is_actionable_message(&msg));
    }

    #[test]
    fn test_is_actionable_message_false_for_unsupported() {
        let msg = make_msg(ImMessageContent::Unsupported {
            message_type: "location".to_string(),
            raw_content: "{}".to_string(),
        });
        assert!(!is_actionable_message(&msg));
    }
}
