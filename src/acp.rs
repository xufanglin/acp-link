use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;

use agent_client_protocol::{
    Agent, Client, ClientSideConnection, ContentBlock, ImageContent, Implementation,
    InitializeRequest, LoadSessionRequest, NewSessionRequest, PermissionOptionKind, PromptRequest,
    ProtocolVersion, RequestPermissionOutcome, RequestPermissionRequest, RequestPermissionResponse,
    SelectedPermissionOutcome, SessionId, SessionNotification, SessionUpdate, TextContent,
};
use anyhow::{Context, Result};
use tokio::sync::{Mutex, mpsc, oneshot};
use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};

use crate::config::KiroConfig;

/// 流式事件：文本 chunk 或 tool call 状态
#[derive(Debug, Clone)]
pub enum StreamEvent {
    /// 文本增量
    Text(String),
    /// 工具调用中（显示标题让用户知道正在做什么）
    ToolCall(String),
}

/// FNV-1a 64 位稳定哈希（跨 Rust 版本和进程重启结果完全一致）
fn stable_hash(s: &str) -> u64 {
    let mut hash: u64 = 14695981039346656037;
    for b in s.bytes() {
        hash ^= b as u64;
        hash = hash.wrapping_mul(1099511628211);
    }
    hash
}

/// 从权限选项列表中选择优先级最高的选项 ID
///
/// 优先级：AllowAlways > AllowOnce > 第一个选项
/// 列表为空时返回 `None`。
fn select_permission_option(
    options: &[agent_client_protocol::PermissionOption],
) -> Option<agent_client_protocol::PermissionOptionId> {
    options
        .iter()
        .find(|o| matches!(o.kind, PermissionOptionKind::AllowAlways))
        .or_else(|| {
            options
                .iter()
                .find(|o| matches!(o.kind, PermissionOptionKind::AllowOnce))
        })
        .or(options.first())
        .map(|o| o.option_id.clone())
}

/// ACP Client 回调实现（`!Send`，运行在专用线程的 `LocalSet` 内）
///
/// - 权限请求：自动选择 AllowAlways > AllowOnce > 第一个选项
/// - 会话通知：将 agent 响应文本 chunk 转发到 `chunk_tx` channel
struct AcpClientHandler {
    /// 当前活跃 prompt 的 chunk 发送端，prompt 结束时置为 None
    chunk_tx: Rc<RefCell<Option<mpsc::UnboundedSender<StreamEvent>>>>,
}

#[async_trait::async_trait(?Send)]
impl Client for AcpClientHandler {
    async fn request_permission(
        &self,
        args: RequestPermissionRequest,
    ) -> agent_client_protocol::Result<RequestPermissionResponse> {
        let option_id = select_permission_option(&args.options)
            .ok_or_else(agent_client_protocol::Error::internal_error)?;

        tracing::debug!("自动批准权限请求: {}", option_id.0);

        Ok(RequestPermissionResponse::new(
            RequestPermissionOutcome::Selected(SelectedPermissionOutcome::new(option_id)),
        ))
    }

    async fn session_notification(
        &self,
        args: SessionNotification,
    ) -> agent_client_protocol::Result<()> {
        match &args.update {
            SessionUpdate::AgentMessageChunk(chunk) => {
                if let ContentBlock::Text(text) = &chunk.content {
                    tracing::trace!("chunk: {:?}", text.text);
                    if let Some(tx) = self.chunk_tx.borrow().as_ref() {
                        let _ = tx.send(StreamEvent::Text(text.text.clone()));
                    }
                }
            }
            SessionUpdate::ToolCall(tc) => {
                let title = if tc.title.is_empty() {
                    "工具调用".to_string()
                } else {
                    tc.title.clone()
                };
                tracing::debug!("工具调用: {title} ({})", tc.tool_call_id.0);
                if let Some(tx) = self.chunk_tx.borrow().as_ref() {
                    let _ = tx.send(StreamEvent::ToolCall(title));
                }
            }
            _ => {}
        }
        Ok(())
    }
}

/// 主线程与 ACP 工作线程之间的命令协议
enum AcpCommand {
    NewSession {
        cwd: PathBuf,
        reply: oneshot::Sender<Result<String>>,
    },
    LoadSession {
        session_id: String,
        cwd: PathBuf,
        reply: oneshot::Sender<Result<String>>,
    },
    Prompt {
        session_id: String,
        content: Vec<ContentBlock>,
        reply: oneshot::Sender<Result<mpsc::UnboundedReceiver<StreamEvent>>>,
    },
}

/// ACP 事件循环：在专用线程的 `LocalSet` 内运行，
/// 负责启动 kiro-cli 子进程、初始化 ACP 连接、处理来自主线程的命令。
///
/// 初始化完成后通过 `ready_tx` 发送就绪信号；初始化失败时发送错误。
async fn acp_event_loop(
    worker_id: usize,
    config: KiroConfig,
    mut cmd_rx: mpsc::Receiver<AcpCommand>,
    ready_tx: oneshot::Sender<Result<()>>,
) -> Result<()> {
    tracing::info!(
        "[worker-{worker_id}] 启动 kiro-cli: {} {:?}",
        config.cmd,
        config.args
    );

    let mut child = tokio::process::Command::new(&config.cmd)
        .args(&config.args)
        .current_dir(crate::config::AppConfig::temp_dir())
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::inherit())
        .spawn()
        .with_context(|| format!("启动 kiro-cli 失败: {}", config.cmd))?;

    let stdin = child.stdin.take().context("获取 kiro-cli stdin 失败")?;
    let stdout = child.stdout.take().context("获取 kiro-cli stdout 失败")?;

    let chunk_tx = Rc::new(RefCell::new(None::<mpsc::UnboundedSender<StreamEvent>>));
    let handler = AcpClientHandler {
        chunk_tx: chunk_tx.clone(),
    };

    let (conn, io_task) =
        ClientSideConnection::new(handler, stdin.compat_write(), stdout.compat(), |fut| {
            tokio::task::spawn_local(fut);
        });

    tokio::task::spawn_local(async move {
        if let Err(e) = io_task.await {
            tracing::error!("[worker-{worker_id}] ACP I/O 错误: {e:?}");
        }
    });

    let init_req = InitializeRequest::new(ProtocolVersion::LATEST)
        .client_info(Implementation::new("acp-link", env!("CARGO_PKG_VERSION")));

    let init_result = conn
        .initialize(init_req)
        .await
        .map_err(|e| anyhow::anyhow!("ACP 初始化失败: {e:?}"));

    let init_resp = match init_result {
        Ok(resp) => {
            let _ = ready_tx.send(Ok(()));
            resp
        }
        Err(e) => {
            let _ = ready_tx.send(Err(anyhow::anyhow!("{e}")));
            return Err(e);
        }
    };

    let caps = &init_resp.agent_capabilities;
    tracing::info!(
        "[worker-{worker_id}] ACP 已连接: protocol_version={:?}, agent={:?}, prompt_caps=[image={}, audio={}, embedded_context={}]",
        init_resp.protocol_version,
        init_resp.agent_info,
        caps.prompt_capabilities.image,
        caps.prompt_capabilities.audio,
        caps.prompt_capabilities.embedded_context,
    );

    while let Some(cmd) = cmd_rx.recv().await {
        match cmd {
            AcpCommand::NewSession { cwd, reply } => {
                let result = conn
                    .new_session(NewSessionRequest::new(cwd))
                    .await
                    .map(|r| {
                        let sid = r.session_id.0.to_string();
                        tracing::debug!("[worker-{worker_id}] ACP session 已创建: {sid}");
                        sid
                    })
                    .map_err(|e| anyhow::anyhow!("创建 session 失败: {e:?}"));
                let _ = reply.send(result);
            }
            AcpCommand::LoadSession {
                session_id,
                cwd,
                reply,
            } => {
                let sid = SessionId::new(session_id.as_str());
                let result = conn
                    .load_session(LoadSessionRequest::new(sid, cwd))
                    .await
                    .map(|_| {
                        tracing::debug!("[worker-{worker_id}] ACP session 已加载: {session_id}");
                        session_id
                    })
                    .map_err(|e| anyhow::anyhow!("加载 session 失败: {e:?}"));
                let _ = reply.send(result);
            }
            AcpCommand::Prompt {
                session_id,
                content,
                reply,
            } => {
                let (tx, rx) = mpsc::unbounded_channel();
                *chunk_tx.borrow_mut() = Some(tx);
                let sid = SessionId::new(session_id.as_str());

                // receiver 先发回调用方，使其可立即消费 chunk
                let _ = reply.send(Ok(rx));

                let result = conn.prompt(PromptRequest::new(sid, content)).await;
                // prompt 结束，drop sender 通知调用方流已结束
                *chunk_tx.borrow_mut() = None;

                match result {
                    Ok(resp) => {
                        tracing::debug!(
                            "[worker-{worker_id}] ACP prompt 完成: stop_reason={:?}",
                            resp.stop_reason
                        );
                    }
                    Err(e) => {
                        tracing::error!("[worker-{worker_id}] ACP prompt 失败: {e:?}");
                    }
                }
            }
        }
    }

    // 确保子进程退出（tokio Child::drop 不会 kill 子进程）
    let _ = child.kill().await;
    Ok(())
}

/// 启动单个 ACP worker 线程，返回命令发送端和初始化就绪信号接收端
///
/// 每个 worker 拥有独立的 kiro-cli 子进程、`current_thread` runtime 和 `LocalSet`，
/// 通过 channel 接收来自主线程的命令并串行执行。
fn spawn_worker(
    worker_id: usize,
    config: &KiroConfig,
) -> Result<(mpsc::Sender<AcpCommand>, oneshot::Receiver<Result<()>>)> {
    let (cmd_tx, cmd_rx) = mpsc::channel(32);
    let (ready_tx, ready_rx) = oneshot::channel();
    let config = config.clone();

    std::thread::Builder::new()
        .name(format!("acp-worker-{worker_id}"))
        .spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("创建 ACP runtime 失败");
            let local = tokio::task::LocalSet::new();

            local.block_on(&rt, async {
                if let Err(e) = acp_event_loop(worker_id, config, cmd_rx, ready_tx).await {
                    tracing::error!("[worker-{worker_id}] ACP 工作线程退出: {e}");
                }
            });
        })
        .with_context(|| format!("启动 ACP 工作线程 {worker_id} 失败"))?;

    Ok((cmd_tx, ready_rx))
}

/// 启动 keepalive worker 并等待其就绪（10 秒超时）
async fn spawn_and_wait_keepalive(config: &KiroConfig) -> Result<mpsc::Sender<AcpCommand>> {
    let (tx, ready_rx) = spawn_worker(usize::MAX, config)?;
    match tokio::time::timeout(tokio::time::Duration::from_secs(10), ready_rx).await {
        Err(_) => anyhow::bail!("keepalive worker 初始化超时"),
        Ok(Err(_)) => anyhow::bail!("keepalive worker 已退出"),
        Ok(Ok(Err(e))) => anyhow::bail!("keepalive worker 初始化失败: {e}"),
        Ok(Ok(Ok(()))) => Ok(tx),
    }
}

/// 执行一次 keepalive 心跳：创建临时 session → 发送轻量 prompt → 消费响应
async fn keepalive_once(tx: &mpsc::Sender<AcpCommand>, cwd: &std::path::Path) -> Result<()> {
    // 1) 创建临时 session
    let (reply_tx, reply_rx) = oneshot::channel();
    tx.send(AcpCommand::NewSession {
        cwd: cwd.to_owned(),
        reply: reply_tx,
    })
    .await
    .map_err(|_| anyhow::anyhow!("keepalive worker 已断开"))?;

    let session_id = reply_rx
        .await
        .map_err(|_| anyhow::anyhow!("keepalive worker 已退出"))?
        .context("创建 keepalive session 失败")?;

    // 2) 发送轻量 prompt
    let (prompt_tx, prompt_rx) = oneshot::channel();
    tx.send(AcpCommand::Prompt {
        session_id,
        content: vec![ContentBlock::Text(TextContent::new("hello"))],
        reply: prompt_tx,
    })
    .await
    .map_err(|_| anyhow::anyhow!("keepalive worker 已断开"))?;

    // 3) 消费 chunk receiver 直到结束
    if let Ok(Ok(mut rx)) = prompt_rx.await {
        while rx.recv().await.is_some() {}
    }

    Ok(())
}

/// ACP 桥接：通过 worker 进程池与多个 kiro-cli 进程通信
///
/// 使用 routing key 的稳定 hash 将请求路由到固定的 worker，
/// 保证同一 thread/session 的请求始终由同一个 kiro-cli 处理。
/// worker 崩溃时自动重启并重试一次。
pub struct AcpBridge {
    /// 每个元素对应一个 worker 线程的命令发送端（Mutex 保护以支持重启替换）
    workers: Vec<Arc<Mutex<mpsc::Sender<AcpCommand>>>>,
    /// worker 配置，用于崩溃后重启
    config: KiroConfig,
}

impl AcpBridge {
    /// 启动 kiro-cli 进程池并建立 ACP 连接
    ///
    /// 等待所有 worker 完成初始化（最多 10 秒），确保就绪后才返回。
    pub async fn start(config: &KiroConfig) -> Result<Self> {
        let pool_size = config.pool_size.max(1);
        tracing::info!("启动 ACP 进程池: pool_size={pool_size}");

        let mut workers = Vec::with_capacity(pool_size);
        let mut ready_receivers = Vec::with_capacity(pool_size);

        for i in 0..pool_size {
            let (tx, ready_rx) = spawn_worker(i, config)?;
            workers.push(Arc::new(Mutex::new(tx)));
            ready_receivers.push((i, ready_rx));
        }

        for (i, ready_rx) in ready_receivers {
            match tokio::time::timeout(tokio::time::Duration::from_secs(10), ready_rx).await {
                Err(_) => anyhow::bail!("[worker-{i}] 初始化超时"),
                Ok(Err(_)) => anyhow::bail!("[worker-{i}] 已退出（初始化失败）"),
                Ok(Ok(Err(e))) => anyhow::bail!("[worker-{i}] 初始化失败: {e}"),
                Ok(Ok(Ok(()))) => {
                    tracing::info!("[worker-{i}] 初始化完成");
                }
            }
        }

        let bridge = Self {
            workers,
            config: config.clone(),
        };
        bridge.spawn_keepalive()?;
        Ok(bridge)
    }

    /// 启动专用 keepalive worker 和后台保活任务
    ///
    /// 使用独立的 kiro-cli 进程，每 6 小时发送一次轻量 prompt，
    /// 确保认证 token 不会过期，且不阻塞业务 worker。
    /// worker 崩溃时自动重启，最多连续重试 3 次。
    fn spawn_keepalive(&self) -> Result<()> {
        let config = self.config.clone();
        let temp_dir = crate::config::AppConfig::temp_dir();

        tokio::spawn(async move {
            let mut tx = None::<mpsc::Sender<AcpCommand>>;
            let interval = tokio::time::Duration::from_secs(6 * 3600);
            let max_retries = 3u32;

            // 初始启动
            match spawn_and_wait_keepalive(&config).await {
                Ok(sender) => {
                    tracing::info!("[keepalive] worker 初始化完成");
                    tx = Some(sender);
                }
                Err(e) => {
                    tracing::error!("[keepalive] worker 初始化失败: {e}");
                }
            }

            loop {
                tokio::time::sleep(interval).await;

                // 如果没有 worker，尝试重启
                if tx.is_none() {
                    match spawn_and_wait_keepalive(&config).await {
                        Ok(sender) => {
                            tracing::info!("[keepalive] worker 重启成功");
                            tx = Some(sender);
                        }
                        Err(e) => {
                            tracing::error!("[keepalive] worker 重启失败: {e}");
                            continue;
                        }
                    }
                }

                let sender = tx.as_ref().unwrap();
                tracing::info!("[keepalive] 开始保活心跳");

                let result = keepalive_once(sender, &temp_dir).await;
                match result {
                    Ok(()) => {
                        tracing::info!("[keepalive] 心跳完成");
                    }
                    Err(e) => {
                        tracing::warn!("[keepalive] 心跳失败: {e}，尝试重启 worker");
                        tx = None;

                        // 立即重试（带退避）
                        for attempt in 1..=max_retries {
                            tokio::time::sleep(tokio::time::Duration::from_secs(
                                u64::from(attempt) * 5,
                            ))
                            .await;
                            match spawn_and_wait_keepalive(&config).await {
                                Ok(sender) => {
                                    tracing::info!(
                                        "[keepalive] worker 重启成功 (attempt {attempt})"
                                    );
                                    tx = Some(sender);
                                    break;
                                }
                                Err(e) => {
                                    tracing::error!(
                                        "[keepalive] worker 重启失败 (attempt {attempt}): {e}"
                                    );
                                }
                            }
                        }
                    }
                }
            }
        });

        Ok(())
    }

    /// 根据 routing key 的稳定 hash 选择 worker 索引
    ///
    /// 同一 routing key 始终映射到同一个 worker，保证 session 级别的串行一致性；
    /// 不同 routing key 分散到不同 worker，实现跨会话并行处理。
    fn route_idx(&self, routing_key: &str) -> usize {
        let idx = stable_hash(routing_key) as usize % self.workers.len();
        tracing::debug!("路由 key={routing_key} -> worker-{idx}");
        idx
    }

    /// 向指定 routing key 对应的 worker 发送命令
    ///
    /// 若 worker 已崩溃（send 返回 Err），自动重启 worker 并重试一次。
    async fn send_cmd(&self, routing_key: &str, cmd: AcpCommand) -> Result<()> {
        let idx = self.route_idx(routing_key);

        // 快速路径：clone sender，不长时间持锁
        let sender = self.workers[idx].lock().await.clone();
        match sender.send(cmd).await {
            Ok(()) => return Ok(()),
            Err(tokio::sync::mpsc::error::SendError(cmd)) => {
                // Worker 已崩溃；持锁重启（双重检查：可能另一个任务已完成重启）
                let mut guard = self.workers[idx].lock().await;
                match guard.send(cmd).await {
                    Ok(()) => return Ok(()),
                    Err(tokio::sync::mpsc::error::SendError(cmd)) => {
                        tracing::warn!("[worker-{idx}] 进程已崩溃，正在重启...");
                        let (new_tx, ready_rx) = spawn_worker(idx, &self.config)?;
                        match tokio::time::timeout(tokio::time::Duration::from_secs(10), ready_rx)
                            .await
                        {
                            Err(_) => anyhow::bail!("[worker-{idx}] 重启超时"),
                            Ok(Err(_)) => {
                                anyhow::bail!("[worker-{idx}] 重启后工作线程已退出")
                            }
                            Ok(Ok(Err(e))) => {
                                anyhow::bail!("[worker-{idx}] 重启初始化失败: {e}")
                            }
                            Ok(Ok(Ok(()))) => {}
                        }
                        *guard = new_tx;
                        guard
                            .send(cmd)
                            .await
                            .map_err(|_| anyhow::anyhow!("[worker-{idx}] 重启后发送命令失败"))
                    }
                }
            }
        }
    }

    /// 创建新的 ACP session，返回 session_id
    ///
    /// `routing_key` 用于 hash 路由到固定 worker（调用方传入 thread_id）。
    pub async fn new_session(&self, routing_key: &str, cwd: PathBuf) -> Result<String> {
        let (reply, rx) = oneshot::channel();
        self.send_cmd(routing_key, AcpCommand::NewSession { cwd, reply })
            .await?;
        rx.await
            .map_err(|_| anyhow::anyhow!("ACP 工作线程已退出"))?
    }

    /// 加载已有的 ACP session，返回 session_id
    pub async fn load_session(
        &self,
        routing_key: &str,
        session_id: &str,
        cwd: PathBuf,
    ) -> Result<String> {
        let (reply, rx) = oneshot::channel();
        self.send_cmd(
            routing_key,
            AcpCommand::LoadSession {
                session_id: session_id.to_owned(),
                cwd,
                reply,
            },
        )
        .await?;
        rx.await
            .map_err(|_| anyhow::anyhow!("ACP 工作线程已退出"))?
    }

    /// 发送 prompt 并返回流式 chunk receiver
    ///
    /// receiver 关闭时表示 prompt 完成。
    pub async fn prompt_stream(
        &self,
        routing_key: &str,
        session_id: &str,
        content: Vec<ContentBlock>,
    ) -> Result<mpsc::UnboundedReceiver<StreamEvent>> {
        let (reply, rx) = oneshot::channel();
        self.send_cmd(
            routing_key,
            AcpCommand::Prompt {
                session_id: session_id.to_owned(),
                content,
                reply,
            },
        )
        .await?;
        rx.await
            .map_err(|_| anyhow::anyhow!("ACP 工作线程已退出"))?
    }

    /// 构建文本 ContentBlock
    pub fn text_block(text: &str) -> ContentBlock {
        ContentBlock::Text(TextContent::new(text))
    }

    /// 构建图片 ContentBlock（base64 内嵌）
    pub fn image_block(data: &[u8], mime_type: &str) -> ContentBlock {
        use base64::Engine;
        let b64 = base64::engine::general_purpose::STANDARD.encode(data);
        ContentBlock::Image(ImageContent::new(b64, mime_type))
    }

    /// 构建 ResourceLink ContentBlock
    pub fn resource_link_block(name: &str, uri: &str, mime_type: Option<&str>) -> ContentBlock {
        let mut link = agent_client_protocol::ResourceLink::new(name, uri);
        if let Some(mt) = mime_type {
            link = link.mime_type(mt.to_string());
        }
        ContentBlock::ResourceLink(link)
    }
}

#[cfg(test)]
mod tests {

    use agent_client_protocol::{
        ContentBlock, PermissionOption, PermissionOptionId, PermissionOptionKind,
    };
    use tokio::sync::mpsc;

    use super::*;

    // ── 辅助函数 ────────────────────────────────────────────────────────────

    /// 构造一个 PermissionOption，方便测试使用
    fn make_option(id: &str, kind: PermissionOptionKind) -> PermissionOption {
        PermissionOption::new(PermissionOptionId::new(id), id, kind)
    }

    /// 构造一个 AcpBridge，workers 使用假的 sender（接收端立即丢弃）
    fn make_bridge(pool_size: usize) -> AcpBridge {
        let config = KiroConfig {
            cmd: "false".to_string(),
            args: vec![],
            pool_size,
            cwd: None,
        };
        let workers = (0..pool_size)
            .map(|_| {
                let (tx, _rx) = mpsc::channel::<AcpCommand>(1);
                Arc::new(Mutex::new(tx))
            })
            .collect();
        AcpBridge { workers, config }
    }

    // ── stable_hash ──────────────────────────────────────────────────────────

    #[test]
    fn test_stable_hash_deterministic() {
        // 相同输入应始终产生相同哈希
        assert_eq!(stable_hash("hello"), stable_hash("hello"));
        assert_eq!(stable_hash(""), stable_hash(""));
    }

    #[test]
    fn test_stable_hash_different_inputs() {
        // 不同输入通常产生不同哈希（碰撞极低）
        assert_ne!(stable_hash("thread_a"), stable_hash("thread_b"));
    }

    // ── select_permission_option ─────────────────────────────────────────────

    #[test]
    fn test_select_permission_empty_options_returns_none() {
        // 空列表时应返回 None，避免 panic
        let result = select_permission_option(&[]);
        assert!(result.is_none());
    }

    #[test]
    fn test_select_permission_allow_always_wins() {
        // AllowAlways 应优先于 AllowOnce 和其他选项
        let options = vec![
            make_option("deny", PermissionOptionKind::RejectOnce),
            make_option("once", PermissionOptionKind::AllowOnce),
            make_option("always", PermissionOptionKind::AllowAlways),
        ];
        let id = select_permission_option(&options).unwrap();
        assert_eq!(id.0.as_ref(), "always");
    }

    #[test]
    fn test_select_permission_allow_once_over_deny() {
        // 无 AllowAlways 时，AllowOnce 优先于 Deny
        let options = vec![
            make_option("deny", PermissionOptionKind::RejectOnce),
            make_option("once", PermissionOptionKind::AllowOnce),
        ];
        let id = select_permission_option(&options).unwrap();
        assert_eq!(id.0.as_ref(), "once");
    }

    #[test]
    fn test_select_permission_fallback_to_first() {
        // 无 AllowAlways / AllowOnce 时，选第一个选项
        let options = vec![
            make_option("deny_first", PermissionOptionKind::RejectOnce),
            make_option("deny_second", PermissionOptionKind::RejectOnce),
        ];
        let id = select_permission_option(&options).unwrap();
        assert_eq!(id.0.as_ref(), "deny_first");
    }

    #[test]
    fn test_select_permission_only_allow_always() {
        // 只有 AllowAlways 一项，直接选中
        let options = vec![make_option("always", PermissionOptionKind::AllowAlways)];
        let id = select_permission_option(&options).unwrap();
        assert_eq!(id.0.as_ref(), "always");
    }

    #[test]
    fn test_select_permission_allow_always_first_in_list_wins() {
        // 多个 AllowAlways 时，选列表中第一个出现的
        let options = vec![
            make_option("always_a", PermissionOptionKind::AllowAlways),
            make_option("always_b", PermissionOptionKind::AllowAlways),
        ];
        let id = select_permission_option(&options).unwrap();
        assert_eq!(id.0.as_ref(), "always_a");
    }

    // ── AcpBridge::text_block ────────────────────────────────────────────────

    #[test]
    fn test_text_block_content() {
        // text_block 应构建包含给定文本的 ContentBlock::Text
        let block = AcpBridge::text_block("hello world");
        match block {
            ContentBlock::Text(t) => assert_eq!(t.text, "hello world"),
            other => panic!(
                "期望 ContentBlock::Text，实际: {:?}",
                std::mem::discriminant(&other)
            ),
        }
    }

    #[test]
    fn test_text_block_empty_string() {
        // 空字符串也应正常构建
        let block = AcpBridge::text_block("");
        match block {
            ContentBlock::Text(t) => assert_eq!(t.text, ""),
            other => panic!(
                "期望 ContentBlock::Text，实际: {:?}",
                std::mem::discriminant(&other)
            ),
        }
    }

    #[test]
    fn test_text_block_unicode() {
        // 中文和 emoji 应保持不变
        let block = AcpBridge::text_block("你好 🌍");
        match block {
            ContentBlock::Text(t) => assert_eq!(t.text, "你好 🌍"),
            other => panic!(
                "期望 ContentBlock::Text，实际: {:?}",
                std::mem::discriminant(&other)
            ),
        }
    }

    // ── AcpBridge::image_block ───────────────────────────────────────────────

    #[test]
    fn test_image_block_is_base64_encoded() {
        // image_block 应将原始字节 base64 编码后放入 ContentBlock::Image
        use base64::Engine;
        let data = b"fake_image_data";
        let block = AcpBridge::image_block(data, "image/png");
        match block {
            ContentBlock::Image(img) => {
                assert_eq!(img.mime_type, "image/png");
                // 验证 base64 可正确解码回原始数据
                let decoded = base64::engine::general_purpose::STANDARD
                    .decode(&img.data)
                    .expect("base64 解码失败");
                assert_eq!(decoded, data);
            }
            other => panic!(
                "期望 ContentBlock::Image，实际: {:?}",
                std::mem::discriminant(&other)
            ),
        }
    }

    #[test]
    fn test_image_block_empty_data() {
        // 空数据 base64 编码后应为空字符串
        let block = AcpBridge::image_block(&[], "image/jpeg");
        match block {
            ContentBlock::Image(img) => {
                assert_eq!(img.data, "");
                assert_eq!(img.mime_type, "image/jpeg");
            }
            other => panic!(
                "期望 ContentBlock::Image，实际: {:?}",
                std::mem::discriminant(&other)
            ),
        }
    }

    #[test]
    fn test_image_block_mime_type_preserved() {
        // mime_type 字段应原样保留
        let block = AcpBridge::image_block(&[0u8; 4], "image/webp");
        match block {
            ContentBlock::Image(img) => assert_eq!(img.mime_type, "image/webp"),
            other => panic!(
                "期望 ContentBlock::Image，实际: {:?}",
                std::mem::discriminant(&other)
            ),
        }
    }

    // ── AcpBridge::resource_link_block ──────────────────────────────────────

    #[test]
    fn test_resource_link_block_without_mime() {
        // 不传 mime_type 时，ContentBlock::ResourceLink 应正常构建
        let block = AcpBridge::resource_link_block("report.pdf", "file:///tmp/report.pdf", None);
        match block {
            ContentBlock::ResourceLink(link) => {
                assert_eq!(link.name, "report.pdf");
                assert_eq!(link.uri, "file:///tmp/report.pdf");
                assert!(link.mime_type.is_none());
            }
            other => panic!(
                "期望 ContentBlock::ResourceLink，实际: {:?}",
                std::mem::discriminant(&other)
            ),
        }
    }

    #[test]
    fn test_resource_link_block_with_mime() {
        // 传入 mime_type 时，应正确设置到 link 上
        let block = AcpBridge::resource_link_block(
            "data.json",
            "file:///tmp/data.json",
            Some("application/json"),
        );
        match block {
            ContentBlock::ResourceLink(link) => {
                assert_eq!(link.name, "data.json");
                assert_eq!(link.uri, "file:///tmp/data.json");
                assert_eq!(link.mime_type.as_deref(), Some("application/json"));
            }
            other => panic!(
                "期望 ContentBlock::ResourceLink，实际: {:?}",
                std::mem::discriminant(&other)
            ),
        }
    }

    // ── AcpBridge::route_idx ─────────────────────────────────────────────────

    #[test]
    fn test_route_same_key_always_same_worker() {
        // 相同 routing key 必须路由到同一 worker（确定性）
        let bridge = make_bridge(4);
        let idx_a = bridge.route_idx("thread_abc");
        let idx_b = bridge.route_idx("thread_abc");
        assert_eq!(idx_a, idx_b, "相同 key 应路由到相同 worker");
    }

    #[test]
    fn test_route_different_keys_may_differ() {
        // 不同 key 不要求不同 worker，但路由函数不应 panic
        let bridge = make_bridge(4);
        // 只验证不 panic，以及返回合法 index 范围内的 worker
        let i1 = bridge.route_idx("key_1");
        let i2 = bridge.route_idx("key_2");
        let i3 = bridge.route_idx("");
        assert!(i1 < 4);
        assert!(i2 < 4);
        assert!(i3 < 4);
    }

    #[test]
    fn test_route_single_worker_always_same() {
        // pool_size=1 时，任意 key 都路由到唯一的 worker（index 0）
        let bridge = make_bridge(1);
        assert_eq!(bridge.route_idx("abc"), 0);
        assert_eq!(bridge.route_idx("xyz"), 0);
        assert_eq!(bridge.route_idx(""), 0);
    }

    #[test]
    fn test_route_consistent_across_calls() {
        // 连续多次调用同一 key，index 保持不变
        let bridge = make_bridge(8);
        let key = "persistent_thread_id";
        let first = bridge.route_idx(key);
        for _ in 0..10 {
            assert_eq!(
                bridge.route_idx(key),
                first,
                "路由结果应在多次调用中保持一致"
            );
        }
    }

    #[test]
    fn test_route_idx_within_bounds() {
        // 任意 key 的路由结果都在 [0, pool_size) 范围内
        let bridge = make_bridge(7);
        let keys = ["", "a", "thread_123", "very_long_key_that_might_overflow"];
        for key in keys {
            let idx = bridge.route_idx(key);
            assert!(idx < 7, "索引 {idx} 超出范围 [0, 7)");
        }
    }
}
