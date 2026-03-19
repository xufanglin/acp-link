//! acp-link：IM ↔ ACP (Agent Client Protocol) 桥接服务
//!
//! 监听 IM 消息，通过 ACP 协议转发给 kiro-cli agent，
//! 并将 agent 的流式响应以消息卡片形式回复到 IM。

pub mod config;
pub mod im;
pub mod link;
pub mod mcp;
