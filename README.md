# acp-link

飞书 ↔ ACP (Agent Client Protocol) 桥接服务，使用 Rust 编写。

监听飞书消息，通过 ACP 协议将内容转发给 kiro-cli agent，并将 agent 的流式响应以消息卡片形式回复到飞书。

## 功能特性

- **飞书 WS 长连接监听** — 自动重连、心跳维持、消息去重
- **多媒体消息支持** — 文本、图片、文件、音频、视频、表情包
- **Thread 话题聚合** — 首条消息聚合整个 thread 上下文，后续消息增量追加
- **kiro-cli 进程池** — 通过 thread_id hash 路由，支持并行处理（默认 4 个进程）
- **消息卡片流式更新** — 800ms 节流，流式展示 agent 响应
- **资源文件 SHA256 去重存储** — 避免重复下载相同附件
- **Session 持久化** — JSON 文件存储 thread ↔ session 映射，自动过期清理
- **配置优先级查找** — 环境变量 > 当前目录 > `~/.acp-link/`

## 前置要求

- [kiro-cli](https://kiro.dev/)（需支持 `kiro-cli acp` 模式）
- 飞书自建应用，需开通以下权限：
  - `im:message:receive_v1`（接收消息事件）
  - `im:message`（发送/更新消息）
  - `im:message.p2p_msg`（私聊消息）
  - `im:resource`（下载图片/文件资源）

## 安装

从 [Releases](https://github.com/xufanglin/acp-link/releases) 页面下载对应平台的二进制文件，解压后放到 PATH 可访问的目录（如 `/usr/local/bin/`）即可。

## 配置说明

首次运行时，若未找到配置文件，会自动在 `~/.acp-link/config.toml` 生成默认模板。

**config.toml 示例：**

```toml
log_level = "info"

[feishu]
app_id = "cli_xxxxxxxxxxxxxx"
app_secret = "xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx"

[kiro]
cmd = "kiro-cli"
args = ["acp", "--agent", "lark"]
pool_size = 4          # 进程池大小，按 thread_id hash 路由
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
Description=ACP Link - Feishu to Kiro bridge
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

## 项目结构

```
src/
├── main.rs        # 入口：加载配置 → 初始化日志 → 启动 LinkService
├── lib.rs         # 模块声明
├── config.rs      # 配置结构体及文件查找逻辑（TOML）
├── link.rs        # LinkService：消息调度、流式处理、卡片更新
├── feishu.rs      # 飞书 WS 客户端：消息监听、thread 聚合、REST API
├── acp.rs         # ACP 桥接：kiro-cli 进程池、session 管理、流式响应
├── session.rs     # Session 映射：thread_id ↔ session_id，JSON 持久化
└── resource.rs    # 资源存储：SHA256 去重、文件下载缓存
```

## 技术架构

```
飞书 WS
  │  长连接接收消息事件
  ▼
LinkService (tokio)
  │  每条消息 spawn 独立任务（并发处理）
  ▼
消息处理流程
  ├─ 新 thread：聚合 thread 全部内容（文本+图片+文件）→ 新建 ACP session
  └─ 已有 thread：增量文本 → 加载已有 ACP session
  │
  ▼
AcpBridge（进程池）
  │  hash(thread_id) % pool_size → 路由到对应 kiro-cli 进程
  │  通过 ACP SDK（agent-client-protocol）发送 prompt
  ▼
流式响应收集
  │  800ms 节流 → 更新飞书消息卡片
  ▼
飞书消息卡片（最终结果）
```

**关键技术点：**

- ACP SDK 使用 `!Send` futures，需要 `tokio::task::LocalSet` + `current_thread` runtime 隔离每个 kiro-cli 进程
- `tokio_util::compat` 桥接 tokio 与 futures 的 `AsyncRead/Write` trait
- 图片通过魔数（magic bytes）检测 MIME 类型，文件通过扩展名推断
- Session 过期后自动清理，资源文件同步清理

## License

MIT
