# acp-link

IM ↔ ACP (Agent Client Protocol) 桥接服务，让你在飞书中直接与 AI Agent 对话。

通过飞书 WebSocket 长连接监听消息，经 ACP 协议转发给后端 agent（如 [Kiro](https://kiro.dev/) CLI、Claude 等任意 ACP 兼容 agent），agent 的流式响应会以富文本消息实时回复到飞书。架构设计支持扩展到钉钉、Slack 等其他 IM 平台。

## 功能特性

- 多媒体消息支持 — 文本、图片、文件、音频、视频、表情包
- Topic 话题聚合 — 首条消息聚合整个 topic 上下文，后续消息增量追加
- 消息流式更新 — 实时展示 agent 响应，无需等待完整回复
- 内嵌 MCP Server — 暴露 IM 平台工具（如文件发送）供 agent 反向调用
- Session 持久化 — 自动过期清理，支持断点续聊

## 前置要求

- 支持 ACP 协议的 agent CLI（如 [kiro-cli](https://kiro.dev/)、claude 等）
- 飞书创建和配置机器人应用，并开通相应的权限。可以直接在开放平台里导入[feishu-permission.json](docs/feishu-permission.json)简化权限的配置

## 安装

从 [Releases](https://github.com/xufanglin/acp-link/releases) 页面下载对应平台的二进制文件，解压后放到 PATH 可访问的目录（如 `/usr/local/bin/`），增加执行权限即可。

## 配置说明

首次运行时，若未找到配置文件，会自动在 `~/.acp-link/config.toml` 生成默认模板。

**config.toml 示例：**

```toml
log_level = "info"
# log_retention = 7       # 日志保留天数（默认 7）
# session_retention = 7   # Session 保留天数（默认 7）
# resource_retention = 7  # 资源文件保留天数（默认 7）

[im.feishu]
app_id = "cli_xxxxxxxxxxxxxx"
app_secret = "xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx"

[backend]
cmd = "kiro-cli"
args = ["acp", "--agent", "lark"]
pool_size = 4          # 进程池大小，按 topic_id hash 路由
# cwd = "/path/to/project"  # Agent 工作目录（默认 ~/.acp-link/temp/）

[mcp]
port = 9800            # MCP HTTP Server 监听端口（默认 9800）
```

### 配置查找顺序

1. 环境变量 `ACP_LINK_CONFIG` 指定的路径
2. 当前工作目录下的 `config.toml`
3. `~/.acp-link/config.toml`

## 使用方式

```bash
# 使用默认配置查找逻辑启动
./acp-link

# 指定配置文件路径
ACP_LINK_CONFIG=/path/to/config.toml ./acp-link

# 调整日志级别
# 也可在 config.toml 中设置 log_level = "debug"
RUST_LOG=debug ./acp-link
```

启动后服务会通过飞书 WS 长连接监听消息，无需额外配置 Webhook 回调地址。

## 作为 systemd 用户服务运行

将下载的二进制移动到系统路径：

```bash
sudo install -m 755 acp-link /usr/local/bin/
```

创建 systemd 用户服务文件：

```bash
mkdir -p ~/.config/systemd/user
cat > ~/.config/systemd/user/acp-link.service << 'EOF'
[Unit]
Description=ACP Link - IM to Agent bridge
After=network-online.target
Wants=network-online.target

[Service]
Environment=PATH=%h/.local/bin:/usr/local/bin:/usr/bin:/bin
ExecStart=/usr/local/bin/acp-link
Restart=on-failure
RestartSec=5s

[Install]
WantedBy=default.target
EOF
```

> `%h` 是 systemd specifier，会自动展开为当前用户的 home 目录（相当于 `$HOME`）。
> 这里将 `~/.local/bin` 加入 PATH 是为了让 `kiro-cli` 等安装在用户目录下的工具能被找到。

启用并启动服务：

```bash
systemctl --user daemon-reload
systemctl --user enable --now acp-link.service
```

**重要：开启 linger，确保服务器重启后无需登录即可自动启动：**

```bash
sudo loginctl enable-linger $USER
```

> 如果不开启 linger，user service 只会在你 SSH 登录后才启动，服务器重启后进程不会自动拉起。

查看运行状态和日志：

```bash
# 查看服务状态
systemctl --user status acp-link.service

# 实时查看日志
journalctl --user -u acp-link.service -f
```

停止 / 重启：

```bash
systemctl --user stop acp-link.service
systemctl --user restart acp-link.service
```

## kiro-cli Agent 配置

要让 kiro-cli 能通过 MCP 协议调用飞书工具（如 `feishu_send_file` 回传文件），需要创建自定义 agent 配置。

### 1. 创建 agent

通过 kiro-cli 命令创建 agent：

```bash
# 方式一：直接命令行创建
kiro-cli agent create lark

# 方式二：进入 kiro-cli 交互模式后创建
kiro-cli
/agent create
```

创建后编辑配置文件 `~/.kiro/agents/lark.json`：

```json
{
  "name": "lark",
  "description": "专门用于与飞书交互",
  "mcpServers": {
    "feishu": {
      "type": "streamable-http",
      "url": "http://127.0.0.1:9800/mcp"
    }
  },
  "tools": ["*"],
  "allowedTools": [],
  "resources": ["file://~/.kiro/agents/lark.md"],
  "includeMcpJson": false
}
```

> `mcpServers.feishu` 指向 acp-link 内嵌的 MCP Server，端口需与 `config.toml` 中的 `[mcp] port` 一致。

### 2. 创建 agent 指令文件

`~/.kiro/agents/lark.md`：

```markdown
# 文件输出规范

- 所有生成或转换的文件必须输出到 `~/.acp-link/temp/` 目录，禁止使用 `/tmp` 或其他临时目录
- 使用 `feishu_send_file` 时，`file_path` 也应指向该目录下的文件
```

### 3. 确认 config.toml 中的 backend 配置

```toml
[backend]
cmd = "kiro-cli"
args = ["acp", "--agent", "lark"]
```

`--agent lark` 会让 kiro-cli 加载上述自定义 agent，从而获得飞书 MCP 工具的调用能力。

如需添加其他 MCP Server（如 `markitdown-mcp` 用于文档转换），可在 `mcpServers` 中追加配置。

## MCP 工具

服务启动时会同时在本地启动 MCP HTTP Server（默认端口 `9800`），暴露 IM 平台相关工具供 agent 调用：

| 工具名                | 说明                                                                 |
| --------------------- | -------------------------------------------------------------------- |
| `feishu_send_file`    | 上传并发送文件到飞书会话（图片走 inline，其他走文件附件）            |
| `feishu_get_document` | 获取飞书云文档纯文本内容（支持 docx URL、wiki URL 或裸 document_id） |

## 目录结构

```
~/.acp-link/
├── config.toml       # 配置文件
├── sessions.json     # Session 映射（自动管理）
├── data/             # 资源文件（SHA256 去重）
├── temp/             # Agent 工作目录
└── logs/             # 滚动日志
```

## 技术文档

详细的系统架构、模块设计、协议细节等请参阅 [docs/](docs/) 目录：

- [architecture.md](docs/architecture.md) — 系统架构与模块划分
- [acp-bridge.md](docs/acp-bridge.md) — ACP 桥接层设计
- [feishu-protocol.md](docs/feishu-protocol.md) — 飞书 WS 协议细节
- [extending-im-platform.md](docs/extending-im-platform.md) — 扩展新的 IM 平台

## License

MIT
