//! 飞书平台的 [`IMChannel`](crate::im::IMChannel) 实现
//!
//! 将 `FeishuClient` 封装为 [`FeishuChannel`]，实现 `IMChannel` trait。
//! 各方法直接委托给内部 client，同时负责飞书特有类型
//! （`FeishuMessage` / `MessageContent` / `ThreadSubmission`）
//! 与跨平台统一类型（`ImMessage` / `ImMessageContent` / `TopicSubmission`）之间的转换。
//!
//! ## 消息监听
//!
//! `listen()` 内部创建一个转发 task：
//! 1. 调用 `FeishuClient::listen()` 接收 `FeishuMessage`
//! 2. 通过 `convert_message()` 转换为 `ImMessage`
//! 3. 发送到上层提供的 `mpsc::Sender<ImMessage>`

use async_trait::async_trait;
use tokio::sync::mpsc;

use super::client::{FeishuClient, FeishuMessage, MessageContent, ThreadSubmission};
use crate::im::{FileItem, IMChannel, ImMessage, ImMessageContent, ImageItem, TopicSubmission};

/// 飞书平台的 IMChannel 实现
#[derive(Clone)]
pub struct FeishuChannel {
    client: FeishuClient,
}

impl FeishuChannel {
    pub fn new(app_id: &str, app_secret: &str) -> Self {
        Self {
            client: FeishuClient::new(app_id, app_secret),
        }
    }
}

#[async_trait]
impl IMChannel for FeishuChannel {
    fn platform_name(&self) -> &str {
        "feishu"
    }

    async fn listen(&self, tx: mpsc::Sender<ImMessage>) -> anyhow::Result<()> {
        let (inner_tx, mut inner_rx) = mpsc::channel::<FeishuMessage>(256);
        let forward_handle = tokio::spawn(async move {
            while let Some(msg) = inner_rx.recv().await {
                let im_msg = convert_message(msg);
                if tx.send(im_msg).await.is_err() {
                    break;
                }
            }
        });
        let result = self.client.listen(inner_tx).await;
        forward_handle.abort();
        result
    }

    async fn reply_message(
        &self,
        message_id: &str,
        markdown: &str,
    ) -> anyhow::Result<(String, String)> {
        self.client.reply_card(message_id, markdown).await
    }

    async fn update_message(&self, message_id: &str, markdown: &str) -> anyhow::Result<()> {
        self.client.update_card(message_id, markdown).await
    }

    async fn download_resource(
        &self,
        message_id: &str,
        file_key: &str,
        resource_type: &str,
    ) -> anyhow::Result<Vec<u8>> {
        self.client
            .download_resource(message_id, file_key, resource_type)
            .await
    }

    async fn aggregate_topic(
        &self,
        topic_id: &str,
        chat_id: &str,
    ) -> anyhow::Result<TopicSubmission> {
        let sub = self.client.aggregate_thread(topic_id, chat_id).await?;
        Ok(convert_submission(sub))
    }

    async fn upload_image(&self, file_name: &str, image_data: &[u8]) -> anyhow::Result<String> {
        self.client.upload_image(file_name, image_data).await
    }

    async fn upload_file(&self, file_name: &str, file_data: &[u8]) -> anyhow::Result<String> {
        self.client.upload_file(file_name, file_data).await
    }

    async fn send_image_reply(&self, message_id: &str, image_key: &str) -> anyhow::Result<()> {
        self.client.send_image_reply(message_id, image_key).await
    }

    async fn send_file_reply(&self, message_id: &str, file_key: &str) -> anyhow::Result<()> {
        self.client.send_file_reply(message_id, file_key).await
    }

    async fn send_card(
        &self,
        chat_id: &str,
        chat_type: &str,
        markdown: &str,
    ) -> anyhow::Result<String> {
        self.client.send_card(chat_id, chat_type, markdown).await
    }

    fn mcp_tool_list(&self) -> Vec<serde_json::Value> {
        super::mcp_tools::list()
    }

    async fn mcp_tool_call(
        &self,
        tool_name: &str,
        args: &serde_json::Value,
    ) -> Result<serde_json::Value, String> {
        super::mcp_tools::call(tool_name, args, &self.client).await
    }
}

// ── 类型转换辅助函数 ──────────────────────────────────────────────────────

fn convert_message(msg: FeishuMessage) -> ImMessage {
    ImMessage {
        message_id: msg.message_id,
        chat_id: msg.chat_id,
        chat_type: msg.chat_type,
        sender_id: msg.sender_open_id,
        content: convert_content(msg.content),
        timestamp: msg.timestamp,
        topic_id: msg.root_id,
    }
}

fn convert_content(content: MessageContent) -> ImMessageContent {
    match content {
        MessageContent::Text(s) => ImMessageContent::Text(s),
        MessageContent::Image { image_key } => ImMessageContent::Image { image_key },
        MessageContent::File {
            file_key,
            file_name,
            file_size,
        } => ImMessageContent::File {
            file_key,
            file_name,
            file_size,
        },
        MessageContent::Audio {
            file_key,
            duration_ms,
        } => ImMessageContent::Audio {
            file_key,
            duration_ms,
        },
        MessageContent::Media {
            file_key,
            file_name,
            duration_ms,
            width,
            height,
        } => ImMessageContent::Media {
            file_key,
            file_name,
            duration_ms,
            width,
            height,
        },
        MessageContent::Sticker {
            file_key,
            file_type,
        } => ImMessageContent::Sticker {
            file_key,
            file_type,
        },
        MessageContent::Link { url } => {
            // 飞书云文档链接注入 hint，引导 agent 使用 feishu_get_document 工具获取内容
            if url.contains(".feishu.cn/wiki/") || url.contains(".feishu.cn/docx/") {
                ImMessageContent::Link {
                    url: format!(
                        "{url} [hint: use feishu_get_document tool to fetch this document and wiki]\n"
                    ),
                }
            } else {
                ImMessageContent::Link { url }
            }
        }
        MessageContent::Unsupported {
            message_type,
            raw_content,
        } => ImMessageContent::Unsupported {
            message_type,
            raw_content,
        },
    }
}

fn convert_submission(sub: ThreadSubmission) -> TopicSubmission {
    TopicSubmission {
        topic_id: sub.thread_id,
        chat_id: sub.chat_id,
        texts: sub.texts,
        images: sub
            .images
            .into_iter()
            .map(|i| ImageItem {
                message_id: i.message_id,
                image_key: i.image_key,
            })
            .collect(),
        files: sub
            .files
            .into_iter()
            .map(|f| FileItem {
                message_id: f.message_id,
                file_key: f.file_key,
                file_name: f.file_name,
            })
            .collect(),
        // 飞书云文档链接注入 hint，引导 agent 使用 feishu_get_document 工具获取内容
        links: sub
            .links
            .into_iter()
            .map(|link| {
                if link.contains(".feishu.cn/wiki/") || link.contains(".feishu.cn/docx/") {
                    format!("{link} [hint: use feishu_get_document tool to fetch this document and wiki]\n")
                } else {
                    link
                }
            })
            .collect(),
    }
}
