# 扩展 IM 平台

acp-link 通过 `IMChannel` trait 抽象 IM 平台差异。要接入新的 IM 平台（如钉钉、Slack），需要以下步骤：

## 1. 实现 `IMChannel` trait

在 `src/im/` 下创建新模块目录（如 `src/im/dingtalk/`），实现 `IMChannel` trait 的所有方法：

```rust
use async_trait::async_trait;
use crate::im::{IMChannel, ImMessage, TopicSubmission};
use tokio::sync::mpsc;

pub struct DingtalkChannel { /* ... */ }

#[async_trait]
impl IMChannel for DingtalkChannel {
    fn platform_name(&self) -> &str { "dingtalk" }

    async fn listen(&self, tx: mpsc::Sender<ImMessage>) -> anyhow::Result<()> {
        // 连接钉钉消息源，将收到的消息转换为 ImMessage 发送到 tx
        todo!()
    }

    async fn reply_message(&self, message_id: &str, markdown: &str) -> anyhow::Result<(String, String)> {
        // 回复富文本消息，返回 (new_message_id, topic_id)
        todo!()
    }

    // ... 实现其余方法（update_message, download_resource, aggregate_topic 等）
    // ... 以及 mcp_tool_list / mcp_tool_call 注册平台专属 MCP 工具
}
```

关键方法说明：

- `listen` — 持续监听消息源，将平台消息转换为统一的 `ImMessage`
- `reply_message` / `update_message` — 富文本消息的创建与流式更新（各平台自行决定渲染方式）
- `aggregate_topic` — 聚合 topic 内所有用户消息（用于首次全量上下文构建）
- `mcp_tool_list` / `mcp_tool_call` — 注册和执行平台专属的 MCP 工具

## 2. 注册模块

在 `src/im.rs` 中添加模块声明和 re-export：

```rust
mod dingtalk;
pub use self::dingtalk::DingtalkChannel;
```

同时创建 `src/im/dingtalk.rs` 作为 facade 模块入口（参考 `src/im/feishu.rs`），在 `src/im/dingtalk/` 目录下实现具体逻辑。

## 3. 添加配置

在 `config.rs` 中：

- 添加平台配置结构体（如 `DingtalkConfig`）
- 在 `AppConfig` 中添加对应字段
- 在 `SUPPORTED_PLATFORMS` 数组中加入 `"dingtalk"`

## 4. 注册到 main.rs

在 `main.rs` 的 platform match 中添加分支：

```rust
let channel: Arc<dyn IMChannel> = match config.im_platform.as_str() {
    "feishu" => Arc::new(FeishuChannel::new(...)),
    "dingtalk" => Arc::new(DingtalkChannel::new(...)),
    _ => anyhow::bail!("不支持的 IM 平台: {}", config.im_platform),
};
```

## 5. 配置文件

在 `config.toml` 中设置 `im_platform` 并添加平台配置段：

```toml
im_platform = "dingtalk"

[dingtalk]
app_key = "your_app_key"
app_secret = "your_app_secret"
```
