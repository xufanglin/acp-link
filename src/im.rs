//! IM 抽象层 facade 模块
//!
//! 定义跨平台统一的消息类型和 `IMChannel` trait，
//! 通过 `pub use` 重新导出所有公开类型。

mod feishu;
pub use self::feishu::FeishuChannel;

use async_trait::async_trait;
use tokio::sync::mpsc;

/// 跨平台统一入站消息
#[derive(Debug, Clone)]
pub struct ImMessage {
    /// 消息 ID
    pub message_id: String,
    /// 会话 ID（单聊或群聊）
    pub chat_id: String,
    /// 会话类型
    pub chat_type: String,
    /// 发送者 ID（平台无关）
    pub sender_id: String,
    /// 消息内容
    pub content: ImMessageContent,
    /// 消息时间戳（秒）
    pub timestamp: u64,
    /// Topic ID（对应飞书 thread_id 等平台概念）
    pub topic_id: Option<String>,
}

/// 跨平台统一消息内容
#[derive(Debug, Clone)]
pub enum ImMessageContent {
    /// 纯文本
    Text(String),
    /// 图片
    Image { image_key: String },
    /// 文件
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
    /// 暂不支持的类型
    Unsupported {
        message_type: String,
        raw_content: String,
    },
}

/// Topic 内聚合的用户消息
#[derive(Debug, Clone)]
pub struct TopicSubmission {
    /// Topic ID
    pub topic_id: String,
    /// 会话 ID
    pub chat_id: String,
    /// 文本消息列表
    pub texts: Vec<String>,
    /// 图片列表
    pub images: Vec<ImageItem>,
    /// 文件列表
    pub files: Vec<FileItem>,
}

/// 聚合结果中的图片项
#[derive(Debug, Clone)]
pub struct ImageItem {
    /// 消息 ID
    pub message_id: String,
    /// 图片 key
    pub image_key: String,
}

/// 聚合结果中的文件项
#[derive(Debug, Clone)]
pub struct FileItem {
    /// 消息 ID
    pub message_id: String,
    /// 文件 key
    pub file_key: String,
    /// 文件名
    pub file_name: String,
}

/// IM 平台统一 trait
///
/// 所有 IM 平台（飞书、钉钉、Slack 等）须实现此 trait。
/// 通过 `Arc<dyn IMChannel>` 在多个 tokio task 间共享。
#[async_trait]
pub trait IMChannel: Send + Sync {
    /// 平台标识，如 "feishu"、"dingtalk"、"slack"
    fn platform_name(&self) -> &str;

    /// 连接并持续监听消息，断开时返回 Ok(())，由调用方决定重连
    async fn listen(&self, tx: mpsc::Sender<ImMessage>) -> anyhow::Result<()>;

    /// 回复消息卡片，返回 (new_message_id, topic_id)
    async fn reply_card(
        &self,
        message_id: &str,
        markdown: &str,
    ) -> anyhow::Result<(String, String)>;

    /// 更新已有消息卡片内容
    async fn update_card(&self, message_id: &str, markdown: &str) -> anyhow::Result<()>;

    /// 下载消息中的资源（图片/文件）
    async fn download_resource(
        &self,
        message_id: &str,
        file_key: &str,
        resource_type: &str,
    ) -> anyhow::Result<Vec<u8>>;

    /// 聚合 topic 内所有用户消息
    async fn aggregate_topic(
        &self,
        topic_id: &str,
        chat_id: &str,
    ) -> anyhow::Result<TopicSubmission>;

    /// 上传图片，返回 image_key
    async fn upload_image(&self, file_name: &str, image_data: &[u8]) -> anyhow::Result<String>;

    /// 上传文件，返回 file_key
    async fn upload_file(&self, file_name: &str, file_data: &[u8]) -> anyhow::Result<String>;

    /// 以图片形式回复消息
    async fn send_image_reply(&self, message_id: &str, image_key: &str) -> anyhow::Result<()>;

    /// 以文件形式回复消息
    async fn send_file_reply(&self, message_id: &str, file_key: &str) -> anyhow::Result<()>;

    /// 返回 MCP tool schema 列表
    fn mcp_tool_list(&self) -> Vec<serde_json::Value>;

    /// 执行 MCP tool 调用
    async fn mcp_tool_call(
        &self,
        tool_name: &str,
        args: &serde_json::Value,
    ) -> Result<serde_json::Value, String>;
}
