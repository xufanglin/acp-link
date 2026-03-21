use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Context;
use futures_util::{SinkExt, StreamExt};
use prost::Message as ProstMessage;
use serde::Deserialize;
use tokio::sync::RwLock;
use tokio_tungstenite::tungstenite::Message as WsMsg;

const FEISHU_API_BASE: &str = "https://open.feishu.cn/open-apis";
const FEISHU_WS_BASE: &str = "https://open.feishu.cn";

/// Token 到期前提前刷新的时间裕量
const TOKEN_REFRESH_SKEW: Duration = Duration::from_secs(120);
/// Token 缺省 TTL（当响应中没有 expire 字段时使用）
const DEFAULT_TOKEN_TTL: Duration = Duration::from_secs(7200);
/// WS 心跳超时：超过此时间未收到任何帧则重连
const WS_HEARTBEAT_TIMEOUT: Duration = Duration::from_secs(300);
/// 消息去重窗口
const DEDUP_WINDOW: Duration = Duration::from_secs(30 * 60);

/// 飞书 WS 协议 protobuf 帧头（key-value）
#[derive(Clone, PartialEq, prost::Message)]
struct PbHeader {
    #[prost(string, tag = "1")]
    pub key: String,
    #[prost(string, tag = "2")]
    pub value: String,
}

/// 飞书 WS 帧。method=0 → CONTROL（ping/pong），method=1 → DATA（事件）
#[derive(Clone, PartialEq, prost::Message)]
struct PbFrame {
    #[prost(uint64, tag = "1")]
    pub seq_id: u64,
    #[prost(uint64, tag = "2")]
    pub log_id: u64,
    #[prost(int32, tag = "3")]
    pub service: i32,
    #[prost(int32, tag = "4")]
    pub method: i32,
    #[prost(message, repeated, tag = "5")]
    pub headers: Vec<PbHeader>,
    #[prost(bytes = "vec", optional, tag = "8")]
    pub payload: Option<Vec<u8>>,
}

impl PbFrame {
    /// 按 key 查找 header 值，未找到返回空串
    fn header_value<'a>(&'a self, key: &str) -> &'a str {
        self.headers
            .iter()
            .find(|h| h.key == key)
            .map(|h| h.value.as_str())
            .unwrap_or("")
    }
}

/// WS 客户端配置（来自 pong 帧 payload）
#[derive(Debug, Deserialize, Default, Clone)]
struct WsClientConfig {
    #[serde(rename = "PingInterval")]
    ping_interval: Option<u64>,
}

/// WS 连接端点 API 响应
#[derive(Debug, Deserialize)]
struct WsEndpointResp {
    code: i32,
    #[serde(default)]
    msg: Option<String>,
    #[serde(default)]
    data: Option<WsEndpoint>,
}

#[derive(Debug, Deserialize)]
struct WsEndpoint {
    #[serde(rename = "URL")]
    url: String,
    #[serde(rename = "ClientConfig")]
    client_config: Option<WsClientConfig>,
}

/// 飞书事件推送的顶层结构
#[derive(Debug, Deserialize)]
struct FeishuEvent {
    header: FeishuEventHeader,
    event: serde_json::Value,
}

#[derive(Debug, Deserialize)]
struct FeishuEventHeader {
    event_type: String,
}

/// `im.message.receive_v1` 事件 payload
#[derive(Debug, Deserialize)]
struct MsgReceivePayload {
    sender: FeishuSender,
    message: RawFeishuMessage,
}

#[derive(Debug, Deserialize)]
struct FeishuSender {
    sender_id: FeishuSenderId,
    #[serde(default)]
    sender_type: String,
}

#[derive(Debug, Deserialize, Default)]
struct FeishuSenderId {
    open_id: Option<String>,
}

#[derive(Debug, Deserialize)]
struct RawFeishuMessage {
    message_id: String,
    chat_id: String,
    chat_type: String,
    message_type: String,
    #[serde(default)]
    content: String,
    #[serde(default)]
    root_id: Option<String>,
    #[serde(default)]
    mentions: Vec<RawMention>,
}

#[derive(Debug, Deserialize)]
struct RawMention {
    #[serde(default)]
    id: RawMentionId,
}

#[derive(Debug, Deserialize, Default)]
struct RawMentionId {
    /// bot mention 没有 user_id，普通用户有
    #[serde(default)]
    user_id: Option<String>,
}

/// tenant_access_token 缓存，到期前自动刷新
#[derive(Debug, Clone)]
struct CachedToken {
    /// token 原始值
    value: String,
    /// 超过此时刻后需重新获取（= 过期时间 - TOKEN_REFRESH_SKEW）
    refresh_after: Instant,
}

/// 飞书消息内容
#[derive(Debug, Clone)]
pub enum MessageContent {
    /// 纯文本（text / post 富文本已展平）
    Text(String),
    /// 图片
    Image { image_key: String },
    /// 文件（PDF、Word、Excel 等）
    File {
        file_key: String,
        file_name: String,
        file_size: u64,
    },
    /// 音频
    Audio { file_key: String, duration_ms: u32 },
    /// 视频
    Media {
        file_key: String,
        file_name: String,
        duration_ms: u32,
        width: u32,
        height: u32,
    },
    /// 表情包
    Sticker { file_key: String, file_type: String },
    /// 暂不支持的类型，保留原始 content 供上层处理
    Unsupported {
        message_type: String,
        raw_content: String,
    },
}

/// 从飞书收到的消息
#[derive(Debug, Clone)]
pub struct FeishuMessage {
    /// 飞书原始 message_id
    pub message_id: String,
    /// 会话 ID（单聊或群聊）
    pub chat_id: String,
    /// 会话类型："p2p" 或 "group"
    pub chat_type: String,
    /// 发送者 open_id
    pub sender_open_id: String,
    /// 消息内容
    pub content: MessageContent,
    /// 消息创建时间戳（秒）
    pub timestamp: u64,
    /// Thread 根消息 ID（非空表示该消息在 Thread 内）
    pub root_id: Option<String>,
}

/// Thread 聚合提交结果
#[derive(Debug, Clone)]
pub struct ThreadSubmission {
    pub thread_id: String,
    pub chat_id: String,
    pub texts: Vec<String>,
    pub images: Vec<ImageItem>,
    pub files: Vec<FileItem>,
}

/// 聚合结果中的图片项
#[derive(Debug, Clone)]
pub struct ImageItem {
    pub message_id: String,
    pub image_key: String,
}

/// 聚合结果中的文件项
#[derive(Debug, Clone)]
pub struct FileItem {
    pub message_id: String,
    pub file_key: String,
    pub file_name: String,
}

/// 飞书客户端（仅中国区）
///
/// # Examples
///
/// ```ignore
/// use tokio::sync::mpsc;
/// use acp_link::feishu::FeishuClient;
///
/// #[tokio::main]
/// async fn main() {
///     let client = FeishuClient::new("cli_xxx", "your_app_secret");
///     let (tx, mut rx) = mpsc::channel(32);
///
///     tokio::spawn({
///         let client = client.clone();
///         async move {
///             loop {
///                 if let Err(e) = client.listen(tx.clone()).await {
///                     tracing::error!("WS 断开: {e}，5 秒后重连");
///                     tokio::time::sleep(std::time::Duration::from_secs(5)).await;
///                 }
///             }
///         }
///     });
///
///     while let Some(msg) = rx.recv().await {
///         println!("[{}] {}: {:?}", msg.chat_id, msg.sender_open_id, msg.content);
///     }
/// }
/// ```
#[derive(Clone)]
pub struct FeishuClient {
    app_id: String,
    app_secret: String,
    /// 共享 HTTP 客户端，复用连接池避免重复 TLS 握手
    http: reqwest::Client,
    /// 缓存的 tenant_access_token，过期后自动刷新
    tenant_token: Arc<RwLock<Option<CachedToken>>>,
    /// 消息 ID 去重窗口（防止 WS 重连后重复处理）
    seen_ids: Arc<RwLock<HashMap<String, Instant>>>,
}

impl FeishuClient {
    /// 创建飞书客户端
    pub fn new(app_id: &str, app_secret: &str) -> Self {
        Self {
            app_id: app_id.to_owned(),
            app_secret: app_secret.to_owned(),
            http: reqwest::Client::new(),
            tenant_token: Arc::new(RwLock::new(None)),
            seen_ids: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// 连接 WS 并持续接收消息，发送到 `tx`。
    ///
    /// 连接断开（正常关闭、超时、读写错误）时返回 `Ok(())`，
    /// 由调用方决定是否重连。仅在内部逻辑错误时返回 `Err`。
    pub async fn listen(&self, tx: tokio::sync::mpsc::Sender<FeishuMessage>) -> anyhow::Result<()> {
        let (wss_url, client_config) = self.get_ws_endpoint().await?;
        let service_id = parse_service_id(&wss_url);
        tracing::info!("飞书 WS: 连接 {wss_url}");

        let (ws_stream, _) = tokio_tungstenite::connect_async(&wss_url)
            .await
            .context("WS 连接失败")?;
        let (mut write, mut read) = ws_stream.split();
        tracing::info!("飞书 WS: 已连接 (service_id={service_id})");

        let mut ping_secs = client_config.ping_interval.unwrap_or(120).max(10);
        let mut hb_interval = tokio::time::interval(Duration::from_secs(ping_secs));
        let mut timeout_check = tokio::time::interval(Duration::from_secs(10));
        hb_interval.tick().await;

        let mut seq: u64 = 0;
        let mut last_recv = Instant::now();

        seq = seq.wrapping_add(1);
        let initial_ping = build_ping(seq, service_id);
        if write
            .send(WsMsg::Binary(initial_ping.encode_to_vec().into()))
            .await
            .is_err()
        {
            anyhow::bail!("飞书 WS: 初始 ping 发送失败");
        }

        type FragEntry = (Vec<Option<Vec<u8>>>, Instant);
        let mut frag_cache: HashMap<String, FragEntry> = HashMap::new();

        loop {
            tokio::select! {
                biased;

                _ = hb_interval.tick() => {
                    seq = seq.wrapping_add(1);
                    let ping = build_ping(seq, service_id);
                    if write.send(WsMsg::Binary(ping.encode_to_vec().into())).await.is_err() {
                        tracing::warn!("飞书 WS: ping 发送失败，重连");
                        break;
                    }
                    let cutoff = Instant::now()
                        .checked_sub(Duration::from_secs(300))
                        .unwrap_or_else(Instant::now);
                    frag_cache.retain(|_, (_, ts)| *ts > cutoff);
                }

                _ = timeout_check.tick() => {
                    if last_recv.elapsed() > WS_HEARTBEAT_TIMEOUT {
                        tracing::warn!("飞书 WS: 心跳超时，重连");
                        break;
                    }
                }

                msg = read.next() => {
                    let raw = match msg {
                        Some(Ok(ws_msg)) => {
                            if is_live_frame(&ws_msg) {
                                last_recv = Instant::now();
                            }
                            match ws_msg {
                                WsMsg::Binary(b) => b,
                                WsMsg::Ping(d) => {
                                    let _ = write.send(WsMsg::Pong(d)).await;
                                    continue;
                                }
                                WsMsg::Close(_) => {
                                    tracing::info!("飞书 WS: 服务端关闭，重连");
                                    break;
                                }
                                _ => continue,
                            }
                        }
                        None => { tracing::info!("飞书 WS: 连接关闭，重连"); break; }
                        Some(Err(e)) => { tracing::error!("飞书 WS: 读取错误: {e}"); break; }
                    };

                    let frame = match PbFrame::decode(&raw[..]) {
                        Ok(f) => f,
                        Err(e) => { tracing::error!("飞书 WS: proto 解码失败: {e}"); continue; }
                    };

                    if frame.method == 0 {
                        if frame.header_value("type") == "pong" {
                            if let Some(p) = &frame.payload {
                                if let Ok(cfg) = serde_json::from_slice::<WsClientConfig>(p) {
                                    if let Some(secs) = cfg.ping_interval {
                                        let secs = secs.max(10);
                                        if secs != ping_secs {
                                            ping_secs = secs;
                                            hb_interval = tokio::time::interval(
                                                Duration::from_secs(ping_secs),
                                            );
                                            tracing::info!("飞书 WS: ping_interval → {ping_secs}s");
                                        }
                                    }
                                }
                            }
                        }
                        continue;
                    }

                    let msg_type = frame.header_value("type").to_string();
                    let msg_id   = frame.header_value("message_id").to_string();
                    let sum      = frame.header_value("sum").parse::<usize>().unwrap_or(1);
                    let seq_num  = frame.header_value("seq").parse::<usize>().unwrap_or(0);

                    // 飞书要求 3 秒内 ACK
                    {
                        let mut ack = frame.clone();
                        ack.payload = Some(br#"{"code":200,"headers":{},"data":[]}"#.to_vec());
                        ack.headers.push(PbHeader { key: "biz_rt".into(), value: "0".into() });
                        let _ = write.send(WsMsg::Binary(ack.encode_to_vec().into())).await;
                    }

                    let sum = if sum == 0 { 1 } else { sum };
                    let payload: Vec<u8> = if sum == 1 || msg_id.is_empty() || seq_num >= sum {
                        frame.payload.clone().unwrap_or_default()
                    } else {
                        let entry = frag_cache
                            .entry(msg_id.clone())
                            .or_insert_with(|| (vec![None; sum], Instant::now()));
                        if entry.0.len() != sum {
                            *entry = (vec![None; sum], Instant::now());
                        }
                        entry.0[seq_num] = frame.payload.clone();
                        if entry.0.iter().all(|s| s.is_some()) {
                            let full: Vec<u8> = entry
                                .0
                                .iter()
                                .flat_map(|s| s.as_deref().unwrap_or(&[]))
                                .copied()
                                .collect();
                            frag_cache.remove(&msg_id);
                            full
                        } else {
                            continue;
                        }
                    };

                    if msg_type != "event" {
                        continue;
                    }

                    let event: FeishuEvent = match serde_json::from_slice(&payload) {
                        Ok(e) => e,
                        Err(e) => { tracing::error!("飞书 WS: 事件 JSON 解析失败: {e}"); continue; }
                    };
                    if event.header.event_type != "im.message.receive_v1" {
                        continue;
                    }

                    let recv: MsgReceivePayload = match serde_json::from_value(event.event) {
                        Ok(r) => r,
                        Err(e) => { tracing::error!("飞书 WS: 消息体解析失败: {e}"); continue; }
                    };

                    if matches!(recv.sender.sender_type.as_str(), "app" | "bot") {
                        continue;
                    }

                    let sender_open_id =
                        recv.sender.sender_id.open_id.clone().unwrap_or_default();
                    let raw_msg = recv.message;

                    {
                        let now = Instant::now();
                        let mut seen = self.seen_ids.write().await;
                        seen.retain(|_, t| now.duration_since(*t) < DEDUP_WINDOW);
                        if seen.contains_key(&raw_msg.message_id) {
                            tracing::debug!("飞书 WS: 重复消息 {}", raw_msg.message_id);
                            continue;
                        }
                        seen.insert(raw_msg.message_id.clone(), now);
                    }

                    // 群聊须 @机器人（bot mention 无 user_id）
                    if raw_msg.chat_type == "group"
                        && !raw_msg.mentions.iter().any(|m| m.id.user_id.is_none())
                    {
                        tracing::debug!(
                            "飞书 WS: 群聊消息未@机器人，跳过 {}",
                            raw_msg.message_id
                        );
                        continue;
                    }

                    let content = match raw_msg.message_type.as_str() {
                        "text" => match parse_text_content(&raw_msg.content) {
                            Some(t) => {
                                let t = strip_at_placeholders(&t);
                                let t = t.trim().to_string();
                                if t.is_empty() { continue; }
                                MessageContent::Text(t)
                            }
                            None => continue,
                        },
                        "post" => {
                            tracing::debug!("飞书 WS: 不支持的消息类型 'post'");
                            MessageContent::Unsupported {
                                message_type: "post".to_string(),
                                raw_content: raw_msg.content.clone(),
                            }
                        }
                        "image"   => parse_image_content(&raw_msg.content),
                        "file"    => parse_file_content(&raw_msg.content),
                        "audio"   => parse_audio_content(&raw_msg.content),
                        "media"   => parse_media_content(&raw_msg.content),
                        "sticker" => parse_sticker_content(&raw_msg.content),
                        other => {
                            tracing::debug!("飞书 WS: 不支持的消息类型 '{other}'");
                            MessageContent::Unsupported {
                                message_type: other.to_string(),
                                raw_content: raw_msg.content.clone(),
                            }
                        }
                    };

                    let timestamp = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_secs();

                    let root_id = raw_msg.root_id.filter(|s| !s.is_empty());

                    let feishu_msg = FeishuMessage {
                        message_id: raw_msg.message_id,
                        chat_id: raw_msg.chat_id,
                        chat_type: raw_msg.chat_type,
                        sender_open_id,
                        content,
                        timestamp,
                        root_id,
                    };

                    tracing::debug!("飞书 WS: 收到消息 chat_id={}", feishu_msg.chat_id);
                    if tx.send(feishu_msg).await.is_err() {
                        tracing::warn!("飞书 WS: 接收端已关闭");
                        break;
                    }
                }
            }
        }

        Ok(())
    }

    /// 获取 WS 连接端点 URL 和客户端配置
    async fn get_ws_endpoint(&self) -> anyhow::Result<(String, WsClientConfig)> {
        let client = self.http.clone();
        let resp = client
            .post(format!("{FEISHU_WS_BASE}/callback/ws/endpoint"))
            .header("locale", "zh")
            .json(&serde_json::json!({
                "AppID": self.app_id,
                "AppSecret": self.app_secret,
            }))
            .send()
            .await
            .context("获取 WS endpoint 请求失败")?
            .json::<WsEndpointResp>()
            .await
            .context("解析 WS endpoint 响应失败")?;

        if resp.code != 0 {
            anyhow::bail!(
                "飞书 WS endpoint 失败: code={} msg={}",
                resp.code,
                resp.msg.as_deref().unwrap_or("(none)")
            );
        }

        let ep = resp
            .data
            .ok_or_else(|| anyhow::anyhow!("飞书 WS endpoint 响应 data 为空"))?;

        Ok((ep.url, ep.client_config.unwrap_or_default()))
    }

    /// 下载消息中的资源文件（文件、图片、音频、视频等）
    ///
    /// # Arguments
    ///
    /// * `message_id` - 消息 ID（来自 [`FeishuMessage::message_id`]）
    /// * `file_key` - 资源 key（来自 [`MessageContent`] 各变体）
    /// * `resource_type` - `"file"` 或 `"image"`
    ///
    /// # Returns
    ///
    /// 原始文件字节
    pub async fn download_resource(
        &self,
        message_id: &str,
        file_key: &str,
        resource_type: &str,
    ) -> anyhow::Result<Vec<u8>> {
        let token = self.get_tenant_access_token().await?;
        let url = format!(
            "{FEISHU_API_BASE}/im/v1/messages/{message_id}/resources/{file_key}?type={resource_type}"
        );
        let resp = self
            .http
            .clone()
            .get(&url)
            .bearer_auth(&token)
            .send()
            .await
            .context("下载资源请求失败")?;

        if !resp.status().is_success() {
            anyhow::bail!("下载资源失败: HTTP {}", resp.status());
        }

        Ok(resp.bytes().await.context("读取资源字节失败")?.to_vec())
    }

    /// 获取飞书云文档的纯文本内容
    ///
    /// # Arguments
    ///
    /// * `document_id` - 文档 ID（从 URL 中提取，如 `https://xxx.feishu.cn/docx/ABC123` 中的 `ABC123`）
    pub async fn get_document_raw_content(&self, document_id: &str) -> anyhow::Result<String> {
        let token = self.get_tenant_access_token().await?;
        let url = format!("{FEISHU_API_BASE}/docx/v1/documents/{document_id}/raw_content");
        let resp: serde_json::Value = self
            .http
            .get(&url)
            .bearer_auth(&token)
            .send()
            .await
            .context("获取云文档内容请求失败")?
            .json()
            .await
            .context("解析云文档响应失败")?;

        let code = resp.get("code").and_then(|c| c.as_i64()).unwrap_or(-1);
        if code != 0 {
            let msg = resp
                .get("msg")
                .and_then(|m| m.as_str())
                .unwrap_or("unknown error");
            anyhow::bail!("获取云文档失败: {msg}");
        }

        let content = resp
            .pointer("/data/content")
            .and_then(|c| c.as_str())
            .unwrap_or("")
            .to_string();

        Ok(content)
    }

    /// 以消息卡片形式回复，卡片内容为 markdown
    ///
    /// # Returns
    ///
    /// `(new_message_id, thread_id)`
    pub async fn reply_card(
        &self,
        message_id: &str,
        markdown: &str,
    ) -> anyhow::Result<(String, String)> {
        let token = self.get_tenant_access_token().await?;
        let url = format!("{FEISHU_API_BASE}/im/v1/messages/{message_id}/reply");
        let card = Self::build_card(markdown);
        let body = serde_json::json!({
            "content": card.to_string(),
            "msg_type": "interactive",
            "reply_in_thread": true,
        });

        let resp: serde_json::Value = self
            .http
            .clone()
            .post(&url)
            .bearer_auth(&token)
            .json(&body)
            .send()
            .await
            .context("回复卡片请求失败")?
            .json()
            .await
            .context("解析回复卡片响应失败")?;

        let code = resp.get("code").and_then(|c| c.as_i64()).unwrap_or(-1);
        if code != 0 {
            let msg = resp
                .get("msg")
                .and_then(|m| m.as_str())
                .unwrap_or("unknown error");
            anyhow::bail!("回复卡片失败: code={code} msg={msg}");
        }

        let new_msg_id = resp
            .pointer("/data/message_id")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();

        let thread_id = resp
            .pointer("/data/thread_id")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();

        tracing::debug!("回复卡片成功: {message_id} -> {new_msg_id}, thread_id={thread_id}");
        Ok((new_msg_id, thread_id))
    }

    /// 更新已有消息的卡片内容
    pub async fn update_card(&self, message_id: &str, markdown: &str) -> anyhow::Result<()> {
        let token = self.get_tenant_access_token().await?;
        let url = format!("{FEISHU_API_BASE}/im/v1/messages/{message_id}");
        let card = Self::build_card(markdown);
        let body = serde_json::json!({
            "content": card.to_string(),
            "msg_type": "interactive",
        });

        let resp: serde_json::Value = self
            .http
            .clone()
            .patch(&url)
            .bearer_auth(&token)
            .json(&body)
            .send()
            .await
            .context("更新卡片请求失败")?
            .json()
            .await
            .context("解析更新卡片响应失败")?;

        let code = resp.get("code").and_then(|c| c.as_i64()).unwrap_or(-1);
        if code != 0 {
            let msg = resp
                .get("msg")
                .and_then(|m| m.as_str())
                .unwrap_or("unknown error");
            anyhow::bail!("更新卡片失败: code={code} msg={msg}");
        }
        Ok(())
    }

    /// 构建飞书消息卡片 JSON（内联 markdown 元素）
    fn build_card(markdown: &str) -> serde_json::Value {
        serde_json::json!({
            "elements": [{
                "tag": "markdown",
                "content": markdown,
            }]
        })
    }

    /// 获取话题（thread）内的所有消息，自动分页
    ///
    /// # Arguments
    ///
    /// * `thread_id` - 话题 ID
    ///
    /// # Returns
    ///
    /// 按时间顺序排列的消息列表（原始 JSON）
    pub async fn get_thread_messages(
        &self,
        thread_id: &str,
    ) -> anyhow::Result<Vec<serde_json::Value>> {
        let token = self.get_tenant_access_token().await?;
        let client = self.http.clone();
        let mut all_messages: Vec<serde_json::Value> = Vec::new();
        let mut page_token: Option<String> = None;

        loop {
            let mut url = format!(
                "{FEISHU_API_BASE}/im/v1/messages?container_id_type=thread&container_id={thread_id}&page_size=50"
            );
            if let Some(ref pt) = page_token {
                url.push_str(&format!("&page_token={pt}"));
            }

            let resp: serde_json::Value = client
                .get(&url)
                .bearer_auth(&token)
                .send()
                .await
                .context("获取话题消息请求失败")?
                .json()
                .await
                .context("解析话题消息响应失败")?;

            let code = resp.get("code").and_then(|c| c.as_i64()).unwrap_or(-1);
            if code != 0 {
                let msg = resp
                    .get("msg")
                    .and_then(|m| m.as_str())
                    .unwrap_or("unknown error");
                anyhow::bail!("获取话题消息失败: code={code} msg={msg}");
            }

            if let Some(items) = resp.pointer("/data/items").and_then(|v| v.as_array()) {
                all_messages.extend(items.iter().cloned());
            }

            let has_more = resp
                .pointer("/data/has_more")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);

            if !has_more {
                break;
            }

            page_token = resp
                .pointer("/data/page_token")
                .and_then(|v| v.as_str())
                .map(String::from);

            if page_token.is_none() {
                break;
            }
        }

        tracing::debug!(
            "获取话题消息完成: thread_id={thread_id}, count={}",
            all_messages.len()
        );
        Ok(all_messages)
    }

    /// 聚合 Thread 内所有用户消息
    ///
    /// 拉取指定 thread 的全部消息，过滤 bot 消息和「确认」关键词，
    /// 按类型分类聚合为 [`ThreadSubmission`]。
    pub async fn aggregate_thread(
        &self,
        thread_id: &str,
        chat_id: &str,
    ) -> anyhow::Result<ThreadSubmission> {
        let chat_id = chat_id.to_string();

        let messages = self.get_thread_messages(thread_id).await?;

        let mut texts = Vec::new();
        let mut images = Vec::new();
        let mut files = Vec::new();

        for msg in &messages {
            let sender_type = msg
                .pointer("/sender/sender_type")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            if matches!(sender_type, "app" | "bot") {
                continue;
            }

            let message_id = msg
                .get("message_id")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let msg_type = msg.get("msg_type").and_then(|v| v.as_str()).unwrap_or("");
            let content_str = msg
                .get("body")
                .and_then(|b| b.get("content"))
                .and_then(|v| v.as_str())
                .unwrap_or("");

            match msg_type {
                "text" => {
                    if let Some(text) = parse_text_content(content_str) {
                        let text = text.trim().to_string();
                        if !text.is_empty() {
                            texts.push(text);
                        }
                    }
                }
                "image" => {
                    let v: serde_json::Value =
                        serde_json::from_str(content_str).unwrap_or_default();
                    let image_key = v["image_key"].as_str().unwrap_or("").to_string();
                    if !image_key.is_empty() {
                        images.push(ImageItem {
                            message_id,
                            image_key,
                        });
                    }
                }
                "file" => {
                    let v: serde_json::Value =
                        serde_json::from_str(content_str).unwrap_or_default();
                    let file_key = v["file_key"].as_str().unwrap_or("").to_string();
                    let file_name = v["file_name"].as_str().unwrap_or("").to_string();
                    if !file_key.is_empty() {
                        files.push(FileItem {
                            message_id,
                            file_key,
                            file_name,
                        });
                    }
                }
                _ => {}
            }
        }

        Ok(ThreadSubmission {
            thread_id: thread_id.to_string(),
            chat_id,
            texts,
            images,
            files,
        })
    }

    /// 上传图片到飞书，返回 image_key
    pub async fn upload_image(&self, file_name: &str, image_data: &[u8]) -> anyhow::Result<String> {
        let token = self.get_tenant_access_token().await?;
        let url = format!("{FEISHU_API_BASE}/im/v1/images");
        let mime = mime_from_ext(file_name);
        let part = reqwest::multipart::Part::bytes(image_data.to_vec())
            .file_name(file_name.to_string())
            .mime_str(&mime)
            .context("设置 MIME type 失败")?;
        let form = reqwest::multipart::Form::new()
            .text("image_type", "message")
            .part("image", part);
        tracing::debug!(file_name, mime, size = image_data.len(), "上传图片到飞书");
        let resp: serde_json::Value = self
            .http
            .clone()
            .post(&url)
            .bearer_auth(&token)
            .multipart(form)
            .send()
            .await
            .context("上传图片请求失败")?
            .json()
            .await
            .context("解析上传图片响应失败")?;
        let code = resp.get("code").and_then(|c| c.as_i64()).unwrap_or(-1);
        if code != 0 {
            let msg = resp
                .get("msg")
                .and_then(|m| m.as_str())
                .unwrap_or("unknown error");
            tracing::warn!(code, msg, "上传图片失败，飞书返回错误");
            anyhow::bail!("上传图片失败: code={code} msg={msg}");
        }
        resp.pointer("/data/image_key")
            .and_then(|v| v.as_str())
            .map(str::to_string)
            .ok_or_else(|| anyhow::anyhow!("响应中缺少 image_key"))
    }

    /// 上传文件到飞书，返回 file_key
    pub async fn upload_file(&self, file_name: &str, file_data: &[u8]) -> anyhow::Result<String> {
        let token = self.get_tenant_access_token().await?;
        let url = format!("{FEISHU_API_BASE}/im/v1/files");
        let form = reqwest::multipart::Form::new()
            .text("file_type", "stream")
            .text("file_name", file_name.to_string())
            .part(
                "file",
                reqwest::multipart::Part::bytes(file_data.to_vec())
                    .file_name(file_name.to_string()),
            );
        let resp: serde_json::Value = self
            .http
            .clone()
            .post(&url)
            .bearer_auth(&token)
            .multipart(form)
            .send()
            .await
            .context("上传文件请求失败")?
            .json()
            .await
            .context("解析上传文件响应失败")?;
        let code = resp.get("code").and_then(|c| c.as_i64()).unwrap_or(-1);
        if code != 0 {
            let msg = resp
                .get("msg")
                .and_then(|m| m.as_str())
                .unwrap_or("unknown error");
            anyhow::bail!("上传文件失败: code={code} msg={msg}");
        }
        resp.pointer("/data/file_key")
            .and_then(|v| v.as_str())
            .map(str::to_string)
            .ok_or_else(|| anyhow::anyhow!("响应中缺少 file_key"))
    }

    /// 以图片消息回复指定消息
    pub async fn send_image_reply(&self, message_id: &str, image_key: &str) -> anyhow::Result<()> {
        let token = self.get_tenant_access_token().await?;
        let url = format!("{FEISHU_API_BASE}/im/v1/messages/{message_id}/reply");
        let content = serde_json::json!({"image_key": image_key}).to_string();
        let body = serde_json::json!({
            "content": content,
            "msg_type": "image",
            "reply_in_thread": true,
        });
        let resp: serde_json::Value = self
            .http
            .clone()
            .post(&url)
            .bearer_auth(&token)
            .json(&body)
            .send()
            .await
            .context("发送图片回复请求失败")?
            .json()
            .await
            .context("解析图片回复响应失败")?;
        let code = resp.get("code").and_then(|c| c.as_i64()).unwrap_or(-1);
        if code != 0 {
            let msg = resp
                .get("msg")
                .and_then(|m| m.as_str())
                .unwrap_or("unknown error");
            anyhow::bail!("发送图片回复失败: code={code} msg={msg}");
        }
        Ok(())
    }

    /// 以文件消息回复指定消息
    pub async fn send_file_reply(&self, message_id: &str, file_key: &str) -> anyhow::Result<()> {
        let token = self.get_tenant_access_token().await?;
        let url = format!("{FEISHU_API_BASE}/im/v1/messages/{message_id}/reply");
        let content = serde_json::json!({"file_key": file_key}).to_string();
        let body = serde_json::json!({
            "content": content,
            "msg_type": "file",
            "reply_in_thread": true,
        });
        let resp: serde_json::Value = self
            .http
            .clone()
            .post(&url)
            .bearer_auth(&token)
            .json(&body)
            .send()
            .await
            .context("发送文件回复请求失败")?
            .json()
            .await
            .context("解析文件回复响应失败")?;
        let code = resp.get("code").and_then(|c| c.as_i64()).unwrap_or(-1);
        if code != 0 {
            let msg = resp
                .get("msg")
                .and_then(|m| m.as_str())
                .unwrap_or("unknown error");
            anyhow::bail!("发送文件回复失败: code={code} msg={msg}");
        }
        Ok(())
    }

    /// 获取（带缓存的）tenant_access_token
    ///
    /// 使用 double-check 模式避免并发刷新：先读锁快速检查，
    /// 未命中时获取写锁并再次检查（可能已被其他任务刷新）。
    pub async fn get_tenant_access_token(&self) -> anyhow::Result<String> {
        // 快速路径：读锁检查缓存
        {
            let cached = self.tenant_token.read().await;
            if let Some(ref token) = *cached {
                if Instant::now() < token.refresh_after {
                    return Ok(token.value.clone());
                }
            }
        }

        // 慢路径：获取写锁，double-check 防止并发重复刷新
        let mut cached = self.tenant_token.write().await;
        if let Some(ref token) = *cached {
            if Instant::now() < token.refresh_after {
                return Ok(token.value.clone());
            }
        }

        let client = self.http.clone();
        let data: serde_json::Value = client
            .post(format!(
                "{FEISHU_API_BASE}/auth/v3/tenant_access_token/internal"
            ))
            .json(&serde_json::json!({
                "app_id": self.app_id,
                "app_secret": self.app_secret,
            }))
            .send()
            .await
            .context("获取 tenant_access_token 请求失败")?
            .json()
            .await
            .context("解析 tenant_access_token 响应失败")?;

        let code = data.get("code").and_then(|c| c.as_i64()).unwrap_or(-1);
        if code != 0 {
            let msg = data
                .get("msg")
                .and_then(|m| m.as_str())
                .unwrap_or("unknown error");
            anyhow::bail!("获取 tenant_access_token 失败: {msg}");
        }

        let token = data
            .get("tenant_access_token")
            .and_then(|t| t.as_str())
            .ok_or_else(|| anyhow::anyhow!("响应中缺少 tenant_access_token 字段"))?
            .to_string();

        let ttl_secs = data
            .get("expire")
            .or_else(|| data.get("expires_in"))
            .and_then(|v| v.as_u64())
            .unwrap_or(DEFAULT_TOKEN_TTL.as_secs())
            .max(1);

        let refresh_after = Instant::now()
            + Duration::from_secs(ttl_secs)
                .checked_sub(TOKEN_REFRESH_SKEW)
                .unwrap_or(Duration::from_secs(1));

        *cached = Some(CachedToken {
            value: token.clone(),
            refresh_after,
        });

        Ok(token)
    }
}

/// 构建 ping 控制帧
fn build_ping(seq: u64, service_id: i32) -> PbFrame {
    PbFrame {
        seq_id: seq,
        log_id: 0,
        service: service_id,
        method: 0,
        headers: vec![PbHeader {
            key: "type".into(),
            value: "ping".into(),
        }],
        payload: None,
    }
}

/// 从 WS URL query string 中提取 service_id
fn parse_service_id(wss_url: &str) -> i32 {
    wss_url
        .split('?')
        .nth(1)
        .and_then(|qs| {
            qs.split('&')
                .find(|kv| kv.starts_with("service_id="))
                .and_then(|kv| kv.split('=').nth(1))
                .and_then(|v| v.parse::<i32>().ok())
        })
        .unwrap_or(0)
}

/// 判断是否为有效的活跃帧（用于心跳超时检测）
fn is_live_frame(msg: &WsMsg) -> bool {
    matches!(msg, WsMsg::Binary(_) | WsMsg::Ping(_) | WsMsg::Pong(_))
}

/// 解析图片消息 JSON：`{"image_key": "..."}`
fn parse_image_content(content: &str) -> MessageContent {
    let v: serde_json::Value = serde_json::from_str(content).unwrap_or_default();
    MessageContent::Image {
        image_key: v["image_key"].as_str().unwrap_or("").to_string(),
    }
}

/// 解析文件消息 JSON：`{"file_key": "...", "file_name": "...", "file_size": N}`
fn parse_file_content(content: &str) -> MessageContent {
    let v: serde_json::Value = serde_json::from_str(content).unwrap_or_default();
    MessageContent::File {
        file_key: v["file_key"].as_str().unwrap_or("").to_string(),
        file_name: v["file_name"].as_str().unwrap_or("").to_string(),
        file_size: v["file_size"].as_u64().unwrap_or(0),
    }
}

/// 解析音频消息 JSON
fn parse_audio_content(content: &str) -> MessageContent {
    let v: serde_json::Value = serde_json::from_str(content).unwrap_or_default();
    MessageContent::Audio {
        file_key: v["file_key"].as_str().unwrap_or("").to_string(),
        duration_ms: v["duration"].as_u64().unwrap_or(0) as u32,
    }
}

/// 解析视频消息 JSON
fn parse_media_content(content: &str) -> MessageContent {
    let v: serde_json::Value = serde_json::from_str(content).unwrap_or_default();
    MessageContent::Media {
        file_key: v["file_key"].as_str().unwrap_or("").to_string(),
        file_name: v["file_name"].as_str().unwrap_or("").to_string(),
        duration_ms: v["duration"].as_u64().unwrap_or(0) as u32,
        width: v["width"].as_u64().unwrap_or(0) as u32,
        height: v["height"].as_u64().unwrap_or(0) as u32,
    }
}

/// 解析表情包消息 JSON
fn parse_sticker_content(content: &str) -> MessageContent {
    let v: serde_json::Value = serde_json::from_str(content).unwrap_or_default();
    MessageContent::Sticker {
        file_key: v["file_key"].as_str().unwrap_or("").to_string(),
        file_type: v["file_type"].as_str().unwrap_or("").to_string(),
    }
}

/// 解析 text 消息内容：`{"text": "..."}`
fn parse_text_content(content: &str) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(content).ok()?;
    v.get("text")
        .and_then(|t| t.as_str())
        .filter(|s| !s.is_empty())
        .map(String::from)
}

/// 根据文件扩展名推断 MIME type
fn mime_from_ext(file_name: &str) -> String {
    let lower = file_name.to_lowercase();
    if lower.ends_with(".png") {
        "image/png"
    } else if lower.ends_with(".jpg") || lower.ends_with(".jpeg") {
        "image/jpeg"
    } else if lower.ends_with(".gif") {
        "image/gif"
    } else if lower.ends_with(".webp") {
        "image/webp"
    } else if lower.ends_with(".bmp") {
        "image/bmp"
    } else {
        "application/octet-stream"
    }
    .to_string()
}

/// 去除飞书群聊中注入的 `@_user_N` 占位符
fn strip_at_placeholders(text: &str) -> String {
    let mut result = String::with_capacity(text.len());
    let mut chars = text.char_indices().peekable();
    while let Some((_, ch)) = chars.next() {
        if ch == '@' {
            let rest: String = chars.clone().map(|(_, c)| c).collect();
            if let Some(after) = rest.strip_prefix("_user_") {
                let skip =
                    "_user_".len() + after.chars().take_while(|c| c.is_ascii_digit()).count();
                // skip 个字符已通过 chars.clone() 计算，逐一消费迭代器跳过
                for _ in 0..skip {
                    chars.next();
                }
                if chars.peek().map(|(_, c)| *c == ' ').unwrap_or(false) {
                    chars.next();
                }
                continue;
            }
        }
        result.push(ch);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_text_content() {
        let content = r#"{"text": "hello world"}"#;
        assert_eq!(parse_text_content(content), Some("hello world".to_string()));
    }

    #[test]
    fn test_parse_text_content_empty() {
        let content = r#"{"text": ""}"#;
        assert_eq!(parse_text_content(content), None);
    }

    #[test]
    fn test_strip_at_placeholders() {
        let text = "hello @_user_1 world @_user_22 done";
        let result = strip_at_placeholders(text);
        assert_eq!(result, "hello world done");
    }

    #[test]
    fn test_strip_at_placeholders_no_change() {
        let text = "@Alice please review";
        let result = strip_at_placeholders(text);
        assert_eq!(result, "@Alice please review");
    }

    #[test]
    fn test_parse_service_id() {
        let url = "wss://example.com/ws?service_id=42&other=val";
        assert_eq!(parse_service_id(url), 42);
    }

    #[test]
    fn test_parse_service_id_missing() {
        let url = "wss://example.com/ws?foo=bar";
        assert_eq!(parse_service_id(url), 0);
    }

    #[test]
    fn test_is_live_frame() {
        assert!(is_live_frame(&WsMsg::Binary(vec![1].into())));
        assert!(is_live_frame(&WsMsg::Ping(vec![].into())));
        assert!(is_live_frame(&WsMsg::Pong(vec![].into())));
        assert!(!is_live_frame(&WsMsg::Text("x".into())));
        assert!(!is_live_frame(&WsMsg::Close(None)));
    }

    // ── parse_text_content 边界情况 ──────────────────────────────────────────

    #[test]
    fn test_parse_text_content_missing_text_field() {
        // JSON 中没有 "text" 字段时应返回 None
        assert_eq!(parse_text_content(r#"{"other": "value"}"#), None);
    }

    #[test]
    fn test_parse_text_content_invalid_json() {
        // 非法 JSON 应返回 None
        assert_eq!(parse_text_content("not json"), None);
    }

    #[test]
    fn test_parse_text_content_whitespace_only() {
        // 只有空白的 text 字段视为非空，原样返回（由调用方 trim 判断）
        let result = parse_text_content(r#"{"text": "  "}"#);
        assert_eq!(result, Some("  ".to_string()));
    }

    // ── strip_at_placeholders 边界情况 ───────────────────────────────────────

    #[test]
    fn test_strip_at_placeholders_at_end_of_string() {
        // @_user_N 出现在字符串末尾，不应 panic
        let text = "hello @_user_5";
        let result = strip_at_placeholders(text);
        assert_eq!(result, "hello ");
    }

    #[test]
    fn test_strip_at_placeholders_multiple_consecutive() {
        // 连续多个 @_user_N 均应被移除
        let text = "@_user_1@_user_2@_user_3";
        let result = strip_at_placeholders(text);
        assert_eq!(result, "");
    }

    #[test]
    fn test_strip_at_placeholders_empty_string() {
        // 空字符串不应 panic
        assert_eq!(strip_at_placeholders(""), "");
    }

    #[test]
    fn test_strip_at_placeholders_at_sign_only() {
        // 单独的 @ 符号不应被移除
        assert_eq!(strip_at_placeholders("@"), "@");
    }

    // ── parse_image_content ──────────────────────────────────────────────────

    #[test]
    fn test_parse_image_content_valid() {
        let content = r#"{"image_key": "img_v2_abc"}"#;
        match parse_image_content(content) {
            MessageContent::Image { image_key } => assert_eq!(image_key, "img_v2_abc"),
            _ => panic!("应解析为 Image"),
        }
    }

    #[test]
    fn test_parse_image_content_empty_json() {
        // 缺少 image_key 时应返回空字符串
        match parse_image_content("{}") {
            MessageContent::Image { image_key } => assert_eq!(image_key, ""),
            _ => panic!("应解析为 Image"),
        }
    }

    // ── parse_file_content ───────────────────────────────────────────────────

    #[test]
    fn test_parse_file_content_valid() {
        let content = r#"{"file_key": "fk_001", "file_name": "report.pdf", "file_size": 12345}"#;
        match parse_file_content(content) {
            MessageContent::File {
                file_key,
                file_name,
                file_size,
            } => {
                assert_eq!(file_key, "fk_001");
                assert_eq!(file_name, "report.pdf");
                assert_eq!(file_size, 12345);
            }
            _ => panic!("应解析为 File"),
        }
    }

    #[test]
    fn test_parse_file_content_missing_fields() {
        // 缺字段时应使用默认值
        match parse_file_content("{}") {
            MessageContent::File {
                file_key,
                file_name,
                file_size,
            } => {
                assert_eq!(file_key, "");
                assert_eq!(file_name, "");
                assert_eq!(file_size, 0);
            }
            _ => panic!("应解析为 File"),
        }
    }

    // ── parse_audio_content ──────────────────────────────────────────────────

    #[test]
    fn test_parse_audio_content_valid() {
        let content = r#"{"file_key": "audio_001", "duration": 3000}"#;
        match parse_audio_content(content) {
            MessageContent::Audio {
                file_key,
                duration_ms,
            } => {
                assert_eq!(file_key, "audio_001");
                assert_eq!(duration_ms, 3000);
            }
            _ => panic!("应解析为 Audio"),
        }
    }

    // ── parse_media_content ──────────────────────────────────────────────────

    #[test]
    fn test_parse_media_content_valid() {
        let content = r#"{"file_key": "vid_001", "file_name": "demo.mp4", "duration": 5000, "width": 1920, "height": 1080}"#;
        match parse_media_content(content) {
            MessageContent::Media {
                file_key,
                file_name,
                duration_ms,
                width,
                height,
            } => {
                assert_eq!(file_key, "vid_001");
                assert_eq!(file_name, "demo.mp4");
                assert_eq!(duration_ms, 5000);
                assert_eq!(width, 1920);
                assert_eq!(height, 1080);
            }
            _ => panic!("应解析为 Media"),
        }
    }

    // ── parse_sticker_content ────────────────────────────────────────────────

    #[test]
    fn test_parse_sticker_content_valid() {
        let content = r#"{"file_key": "stk_001", "file_type": "png"}"#;
        match parse_sticker_content(content) {
            MessageContent::Sticker {
                file_key,
                file_type,
            } => {
                assert_eq!(file_key, "stk_001");
                assert_eq!(file_type, "png");
            }
            _ => panic!("应解析为 Sticker"),
        }
    }

    // ── parse_service_id 边界情况 ────────────────────────────────────────────

    #[test]
    fn test_parse_service_id_no_query_string() {
        // 无查询参数时应返回 0
        assert_eq!(parse_service_id("wss://example.com/ws"), 0);
    }

    #[test]
    fn test_parse_service_id_first_param() {
        // service_id 作为第一个参数
        assert_eq!(parse_service_id("wss://x.com?service_id=99"), 99);
    }

    #[test]
    fn test_parse_service_id_non_numeric() {
        // service_id 值非数字时应返回 0
        assert_eq!(parse_service_id("wss://x.com?service_id=abc"), 0);
    }

    // ── PbFrame::header_value ────────────────────────────────────────────────

    #[test]
    fn test_pbframe_header_value_found() {
        let frame = PbFrame {
            seq_id: 1,
            log_id: 0,
            service: 0,
            method: 0,
            headers: vec![PbHeader {
                key: "type".into(),
                value: "ping".into(),
            }],
            payload: None,
        };
        assert_eq!(frame.header_value("type"), "ping");
    }

    #[test]
    fn test_pbframe_header_value_not_found() {
        // 不存在的 key 应返回空字符串
        let frame = PbFrame {
            seq_id: 1,
            log_id: 0,
            service: 0,
            method: 0,
            headers: vec![],
            payload: None,
        };
        assert_eq!(frame.header_value("missing"), "");
    }

    #[test]
    fn test_pbframe_header_value_multiple_headers() {
        // 多个 header 时应返回第一个匹配的值
        let frame = PbFrame {
            seq_id: 0,
            log_id: 0,
            service: 0,
            method: 1,
            headers: vec![
                PbHeader {
                    key: "sum".into(),
                    value: "3".into(),
                },
                PbHeader {
                    key: "seq".into(),
                    value: "1".into(),
                },
                PbHeader {
                    key: "message_id".into(),
                    value: "msg_abc".into(),
                },
            ],
            payload: None,
        };
        assert_eq!(frame.header_value("sum"), "3");
        assert_eq!(frame.header_value("seq"), "1");
        assert_eq!(frame.header_value("message_id"), "msg_abc");
    }

    // ── PbFrame protobuf 编解码往返 ──────────────────────────────────────────

    #[test]
    fn test_pbframe_encode_decode_roundtrip() {
        use prost::Message as ProstMessage;

        let original = PbFrame {
            seq_id: 42,
            log_id: 99,
            service: 1,
            method: 0,
            headers: vec![PbHeader {
                key: "type".into(),
                value: "ping".into(),
            }],
            payload: Some(b"hello".to_vec()),
        };

        let encoded = original.encode_to_vec();
        let decoded = PbFrame::decode(&encoded[..]).expect("protobuf 解码不应失败");

        assert_eq!(decoded.seq_id, 42);
        assert_eq!(decoded.log_id, 99);
        assert_eq!(decoded.service, 1);
        assert_eq!(decoded.method, 0);
        assert_eq!(decoded.header_value("type"), "ping");
        assert_eq!(decoded.payload, Some(b"hello".to_vec()));
    }

    #[test]
    fn test_pbframe_encode_decode_no_payload() {
        use prost::Message as ProstMessage;

        // payload 为 None 时编解码应保持一致
        let original = PbFrame {
            seq_id: 1,
            log_id: 0,
            service: 0,
            method: 0,
            headers: vec![],
            payload: None,
        };

        let encoded = original.encode_to_vec();
        let decoded = PbFrame::decode(&encoded[..]).expect("protobuf 解码不应失败");
        assert_eq!(decoded.payload, None);
        assert!(decoded.headers.is_empty());
    }

    #[test]
    fn test_pbframe_decode_empty_bytes_gives_default() {
        use prost::Message as ProstMessage;

        // 空字节序列应解码为全零/默认值的帧
        let decoded = PbFrame::decode(&b""[..]).expect("空字节不应返回错误");
        assert_eq!(decoded.seq_id, 0);
        assert_eq!(decoded.method, 0);
        assert!(decoded.headers.is_empty());
        assert_eq!(decoded.payload, None);
    }

    // ── build_ping ───────────────────────────────────────────────────────────

    #[test]
    fn test_build_ping_fields() {
        let frame = build_ping(7, 3);
        assert_eq!(frame.seq_id, 7);
        assert_eq!(frame.service, 3);
        // method=0 表示 CONTROL 帧
        assert_eq!(frame.method, 0);
        assert_eq!(frame.header_value("type"), "ping");
        assert!(frame.payload.is_none());
    }

    #[test]
    fn test_build_ping_seq_zero() {
        // seq=0 时也应正常构建，不 panic
        let frame = build_ping(0, 0);
        assert_eq!(frame.seq_id, 0);
        assert_eq!(frame.header_value("type"), "ping");
    }

    // ── WsClientConfig JSON 反序列化 ─────────────────────────────────────────

    #[test]
    fn test_ws_client_config_deserialize_with_interval() {
        let json = r#"{"PingInterval": 60}"#;
        let cfg: WsClientConfig = serde_json::from_str(json).expect("应成功反序列化");
        assert_eq!(cfg.ping_interval, Some(60));
    }

    #[test]
    fn test_ws_client_config_deserialize_missing_interval() {
        // 字段缺失时应使用 None
        let cfg: WsClientConfig = serde_json::from_str("{}").expect("应成功反序列化");
        assert_eq!(cfg.ping_interval, None);
    }

    // ── FeishuEvent / MsgReceivePayload JSON 反序列化 ─────────────────────────

    #[test]
    fn test_feishu_event_deserialize() {
        let json = r#"{
            "header": {"event_type": "im.message.receive_v1"},
            "event": {"key": "value"}
        }"#;
        let event: FeishuEvent = serde_json::from_str(json).expect("应成功反序列化");
        assert_eq!(event.header.event_type, "im.message.receive_v1");
        assert_eq!(event.event["key"], "value");
    }

    #[test]
    fn test_msg_receive_payload_deserialize() {
        let json = r#"{
            "sender": {
                "sender_id": {"open_id": "ou_abc123"},
                "sender_type": "user"
            },
            "message": {
                "message_id": "om_001",
                "chat_id": "oc_chat001",
                "chat_type": "p2p",
                "message_type": "text",
                "content": "{\"text\": \"hello\"}",
                "root_id": null,
                "mentions": []
            }
        }"#;
        let payload: MsgReceivePayload = serde_json::from_str(json).expect("应成功反序列化");
        assert_eq!(
            payload.sender.sender_id.open_id.as_deref(),
            Some("ou_abc123")
        );
        assert_eq!(payload.sender.sender_type, "user");
        assert_eq!(payload.message.message_id, "om_001");
        assert_eq!(payload.message.chat_type, "p2p");
        assert_eq!(payload.message.message_type, "text");
    }

    #[test]
    fn test_msg_receive_payload_group_with_mention() {
        // 群聊消息带 @mention，mentions[0].id.user_id 为 None 表示 @bot
        let json = r#"{
            "sender": {
                "sender_id": {"open_id": "ou_user1"},
                "sender_type": "user"
            },
            "message": {
                "message_id": "om_002",
                "chat_id": "oc_group1",
                "chat_type": "group",
                "message_type": "text",
                "content": "{\"text\": \"@bot hello\"}",
                "mentions": [{"id": {}}]
            }
        }"#;
        let payload: MsgReceivePayload = serde_json::from_str(json).expect("应成功反序列化");
        assert_eq!(payload.message.chat_type, "group");
        // bot mention 的 user_id 应为 None
        assert!(payload.message.mentions[0].id.user_id.is_none());
    }

    // ── 分片重组纯逻辑 ───────────────────────────────────────────────────────

    #[test]
    fn test_fragment_reassembly_logic() {
        // 模拟 frag_cache 分片重组：3 片，按顺序插入后拼合
        use std::time::Instant;

        type FragEntry = (Vec<Option<Vec<u8>>>, Instant);
        let mut frag_cache: std::collections::HashMap<String, FragEntry> =
            std::collections::HashMap::new();

        let msg_id = "msg_frag_001".to_string();
        let sum = 3usize;

        // 模拟第 0、1、2 片依次到达
        for (seq_num, chunk) in [
            (0usize, b"foo".as_ref()),
            (1, b"bar".as_ref()),
            (2, b"baz".as_ref()),
        ] {
            let entry = frag_cache
                .entry(msg_id.clone())
                .or_insert_with(|| (vec![None; sum], Instant::now()));
            if entry.0.len() != sum {
                *entry = (vec![None; sum], Instant::now());
            }
            entry.0[seq_num] = Some(chunk.to_vec());
        }

        // 所有分片到齐后拼合
        let entry = frag_cache.get(&msg_id).unwrap();
        assert!(entry.0.iter().all(|s| s.is_some()));
        let full: Vec<u8> = entry
            .0
            .iter()
            .flat_map(|s| s.as_deref().unwrap_or(&[]))
            .copied()
            .collect();
        assert_eq!(full, b"foobarbaz");
    }

    #[test]
    fn test_fragment_reassembly_incomplete() {
        // 只到了 2/3 片时，不应触发拼合
        use std::time::Instant;

        type FragEntry = (Vec<Option<Vec<u8>>>, Instant);
        let mut frag_cache: std::collections::HashMap<String, FragEntry> =
            std::collections::HashMap::new();

        let msg_id = "msg_frag_002".to_string();
        let sum = 3usize;

        let entry = frag_cache
            .entry(msg_id.clone())
            .or_insert_with(|| (vec![None; sum], Instant::now()));
        entry.0[0] = Some(b"part0".to_vec());
        entry.0[2] = Some(b"part2".to_vec());
        // 第 1 片还未到

        let entry = frag_cache.get(&msg_id).unwrap();
        // 未全部到达，不应拼合
        assert!(!entry.0.iter().all(|s| s.is_some()));
    }

    // ── root_id 过滤（空串视为 None）────────────────────────────────────────

    #[test]
    fn test_root_id_empty_string_becomes_none() {
        // 飞书有时发空字符串的 root_id，应过滤为 None
        let root_id: Option<String> = Some(String::new()).filter(|s| !s.is_empty());
        assert!(root_id.is_none());
    }

    #[test]
    fn test_root_id_non_empty_preserved() {
        let root_id: Option<String> = Some("om_root_001".to_string()).filter(|s| !s.is_empty());
        assert_eq!(root_id.as_deref(), Some("om_root_001"));
    }

    // ── sum=0 边界：应被规范化为 1 ───────────────────────────────────────────

    #[test]
    fn test_sum_zero_normalized_to_one() {
        // 协议中 sum=0 应当作 sum=1 处理（单帧消息）
        let raw_sum = 0usize;
        let sum = if raw_sum == 0 { 1 } else { raw_sum };
        assert_eq!(sum, 1);
    }
}
