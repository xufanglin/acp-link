//! 飞书平台模块
//!
//! 包含飞书 SDK 客户端、IMChannel 适配层和 MCP Tool 定义。
mod channel;
mod client;
pub(crate) mod mcp_tools;

pub use self::channel::FeishuChannel;
