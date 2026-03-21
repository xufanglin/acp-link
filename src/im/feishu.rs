//! 飞书平台模块
//!
//! 包含飞书 SDK 客户端、[`IMChannel`](crate::im::IMChannel) 适配层和 MCP Tool 定义。
//!
//! ## 模块结构
//!
//! - [`channel`] — [`FeishuChannel`]：实现 `IMChannel` trait，委托给 `FeishuClient`
//! - [`client`] — `FeishuClient`：WS 长连接、protobuf 帧解析、REST API、token 缓存
//! - [`mcp_tools`] — 飞书 MCP Tool 定义（`feishu_send_file`、`feishu_get_document`）
mod channel;
mod client;
pub(crate) mod mcp_tools;

pub use self::channel::FeishuChannel;
