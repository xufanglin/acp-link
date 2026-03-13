//! acp-link：飞书 ↔ ACP (Agent Client Protocol) 桥接服务
//!
//! 监听飞书消息，通过 ACP 协议转发给 kiro-cli agent，
//! 并将 agent 的流式响应以消息卡片形式回复到飞书。

pub mod acp;
pub mod config;
pub mod feishu;
pub mod link;
pub mod mcp;
pub mod resource;
pub mod session;
