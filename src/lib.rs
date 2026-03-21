//! # acp-link
//!
//! IM ↔ ACP (Agent Client Protocol) 桥接服务。
//!
//! 监听 IM 平台消息，通过 ACP 协议转发给 kiro-cli agent，
//! 并将 agent 的流式响应以富文本消息形式实时回复到 IM。
//! 同时内嵌 MCP Server，允许 agent 反向调用 IM 平台能力。
//!
//! ## 模块结构
//!
//! - [`config`] — 配置管理（TOML 解析、优先级查找、目录管理）
//! - [`im`] — IM 抽象层（`IMChannel` trait、统一消息类型、飞书实现）
//! - [`link`] — 核心服务（消息分发、ACP 桥接、session 管理、资源存储）
//! - [`mcp`] — 内嵌 MCP HTTP Server（Streamable HTTP transport）

pub mod config;
pub mod im;
pub mod link;
pub mod mcp;
