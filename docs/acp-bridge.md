# ACP 桥接设计

## 1. 概述

`link/acp.rs` 实现了 acp-link 与 kiro-cli 之间的通信桥接。核心挑战在于：ACP SDK（`agent-client-protocol`）使用了 `!Send` 的 future，无法在 tokio 的多线程调度器上直接使用。为此，桥接层采用**进程池 + 专用线程**架构，将 `!Send` 约束隔离在独立线程内，同时对外暴露线程安全的异步接口。worker 崩溃时支持自动重启。

---

## 2. `!Send` 约束的根源与处理

### 2.1 约束来源

ACP SDK 的 `ClientSideConnection::new()` 要求 `futures::AsyncRead + futures::AsyncWrite`，其内部使用了非 `Send` 的数据结构（如 `Rc`、`RefCell`）。`agent_client_protocol::Client` trait 也以 `async_trait(?Send)` 声明，生成的 future 不实现 `Send`。

### 2.2 解决方案：current_thread + LocalSet

每个 ACP worker 运行在独立的 OS 线程中，该线程内部创建：

```rust
let rt = tokio::runtime::Builder::new_current_thread()
    .enable_all()
    .build()?;
let local = tokio::task::LocalSet::new();
local.block_on(&rt, async {
    acp_event_loop(worker_id, config, cmd_rx, ready_tx).await
});
```

- `current_thread` runtime：所有 task 在同一线程内调度，`!Send` future 永远不会被迁移到其他线程。
- `LocalSet`：提供 `spawn_local`，允许在其内部派生 `!Send` task（如 ACP I/O 任务）。

### 2.3 tokio_util::compat 桥接

kiro-cli 的 stdin/stdout 为 `tokio::io::AsyncWrite/AsyncRead`，而 ACP SDK 要求 `futures::AsyncWrite/AsyncRead`，通过 `tokio-util` 的 compat layer 转换：

```rust
use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};

let (conn, io_task) = ClientSideConnection::new(
    handler,
    stdin.compat_write(),   // tokio AsyncWrite -> futures AsyncWrite
    stdout.compat(),         // tokio AsyncRead  -> futures AsyncRead
    |fut| { tokio::task::spawn_local(fut); },
);
```

`io_task` 同样以 `spawn_local` 派生，保持 `!Send` 约束。

---

## 3. 进程池架构

### 3.1 设计目标

- **并行**：多个用户的对话可以并行进入不同 kiro-cli 进程处理。
- **串行一致性**：同一个 Topic（飞书 Thread 等）的消息必须串行进入同一个 kiro-cli 进程，保证 ACP session 上下文连续。
- **容错**：worker 崩溃时自动重启，对调用方透明。

### 3.2 Worker 结构

```
AcpBridge
├── workers[0]: Arc<Mutex<mpsc::Sender<AcpCommand>>>  →  OS Thread(acp-worker-0)
│                                                          current_thread runtime
│                                                          LocalSet
│                                                          kiro-cli process (PID: X)
│                                                          ClientSideConnection (!Send)
├── workers[1]: Arc<Mutex<mpsc::Sender<AcpCommand>>>  →  OS Thread(acp-worker-1)
│                                                          ...
└── workers[N]: Arc<Mutex<mpsc::Sender<AcpCommand>>>  →  OS Thread(acp-worker-N)
                                                           ...
```

pool_size 由配置文件的 `kiro.pool_size` 控制，默认为 4。每个 worker 的 sender 包裹在 `Arc<Mutex>` 中，崩溃重启时可原子替换。

### 3.3 FNV-1a 稳定 hash 路由

```rust
fn stable_hash(s: &str) -> u64 {
    let mut hash: u64 = 14695981039346656037;
    for b in s.bytes() {
        hash ^= b as u64;
        hash = hash.wrapping_mul(1099511628211);
    }
    hash
}

fn route_idx(&self, routing_key: &str) -> usize {
    stable_hash(routing_key) as usize % self.workers.len()
}
```

使用 FNV-1a 64 位哈希而非 `DefaultHasher`，确保**跨 Rust 版本和进程重启结果完全一致**。`routing_key` 为 `topic_id`（对应飞书 `thread_id` 等平台概念），对同一 topic 的所有请求始终路由到同一个 worker。

```mermaid
graph LR
    T1["topic_id=A\nhash→worker-1"]
    T2["topic_id=B\nhash→worker-0"]
    T3["topic_id=C\nhash→worker-1"]
    T4["topic_id=D\nhash→worker-3"]

    W0["worker-0\nkiro-cli"]
    W1["worker-1\nkiro-cli"]
    W3["worker-3\nkiro-cli"]

    T1 --> W1
    T2 --> W0
    T3 --> W1
    T4 --> W3
```

注意：worker-1 会串行处理 topic A 和 topic C 的请求（队列深度为 32）。

---

## 4. 命令协议

主线程（多线程 runtime）与 worker 线程通过 `AcpCommand` enum 通信：

```rust
enum AcpCommand {
    NewSession {
        cwd: PathBuf,
        reply: oneshot::Sender<Result<String>>,   // 返回 session_id
    },
    LoadSession {
        session_id: String,
        cwd: PathBuf,
        reply: oneshot::Sender<Result<String>>,   // 返回 session_id（确认）
    },
    Prompt {
        session_id: String,
        content: Vec<ContentBlock>,
        reply: oneshot::Sender<Result<mpsc::UnboundedReceiver<String>>>,  // 返回 chunk stream
    },
}
```

### 4.1 Prompt 流式处理时序

```mermaid
sequenceDiagram
    participant L as LinkService (主线程)
    participant W as acp-worker-N (worker 线程)
    participant K as kiro-cli

    L->>W: AcpCommand::Prompt { session_id, content, reply }
    W->>W: 创建 (tx, rx) = unbounded_channel()
    W->>W: chunk_tx = Some(tx)
    W-->>L: reply.send(Ok(rx))   ← 先发回 receiver
    Note over L: 立即开始消费 chunk

    W->>K: ACP Prompt 请求 (stdio)
    loop AgentMessageChunk
        K-->>W: SessionNotification::AgentMessageChunk
        W->>W: chunk_tx.send(text)
        W-->>L: chunk 流向 rx
        L->>L: 追加到 full_text
    end
    K-->>W: Prompt 响应（stop_reason）
    W->>W: chunk_tx = None  ← drop sender，关闭 rx
    Note over L: rx.recv() 返回 None，流结束
```

关键设计：`reply` 在 prompt 开始前就发回（含 `rx`），使主线程可以**立即开始消费** chunk，无需等待整个 prompt 完成。

---

## 5. 权限自动批准

ACP agent 可能在执行工具调用前请求用户权限。`AcpClientHandler::request_permission` 按以下优先级自动选择：

1. `AllowAlways`（永久授权）
2. `AllowOnce`（本次授权）
3. 列表第一个选项（兜底）

```rust
fn select_permission_option(options: &[PermissionOption]) -> Option<PermissionOptionId> {
    options.iter()
        .find(|o| matches!(o.kind, PermissionOptionKind::AllowAlways))
        .or_else(|| options.iter()
            .find(|o| matches!(o.kind, PermissionOptionKind::AllowOnce)))
        .or(options.first())
        .map(|o| o.option_id.clone())
}
```

---

## 6. ContentBlock 构建

`AcpBridge` 提供三个静态辅助方法，供 `link.rs` 构建 prompt 内容：

| 方法                                   | 对应消息类型 | 说明                                         |
| -------------------------------------- | ------------ | -------------------------------------------- |
| `text_block(text)`                     | 文本         | 直接包装为 `ContentBlock::Text`              |
| `image_block(data, mime)`              | 图片         | base64 编码内嵌为 `ContentBlock::Image`      |
| `resource_link_block(name, uri, mime)` | 文件         | `file:///` URI，`ContentBlock::ResourceLink` |

图片使用内嵌（inline）方式，无需 agent 额外下载；文件使用 `file://` URI 让 agent 按需读取本地文件。

---

## 7. Worker 崩溃重启

当 worker 进程退出（kiro-cli 崩溃）时，`mpsc::Sender::send` 会返回错误。`send_cmd` 实现了双重检查重启机制：

```mermaid
flowchart TD
    A[send_cmd] --> B{快速路径: clone sender + send}
    B -->|Ok| Z[返回成功]
    B -->|Err| C[获取 Mutex 独占锁]
    C --> D{二次尝试 send}
    D -->|Ok| Z
    D -->|Err| E[spawn_worker 重启]
    E --> F{等待 ready 信号\n10s 超时}
    F -->|Ok| G[替换 sender]
    G --> H[用新 sender 发送命令]
    H --> Z
    F -->|超时/失败| X[返回错误]
```

双重检查避免多个任务同时检测到崩溃时重复重启同一个 worker。

---

## 8. Keepalive 心跳

`AcpBridge::start()` 在所有业务 worker 就绪后，额外启动一个 keepalive worker（`worker_id = usize::MAX`），专门用于保活。

### 8.1 设计目的

kiro-cli 底层使用的认证 token 有有效期限制。若长时间无用户消息，所有 worker 均空闲，token 可能过期。keepalive worker 定期发送轻量 prompt，触发 kiro-cli 内部的 token 刷新机制。

### 8.2 保活流程

```mermaid
sequenceDiagram
    participant KA as keepalive task (tokio::spawn)
    participant W as keepalive worker (OS Thread)
    participant K as kiro-cli

    loop 每 6 小时
        KA->>W: AcpCommand::NewSession
        W->>K: ACP NewSession
        K-->>W: session_id
        W-->>KA: Ok(session_id)

        KA->>W: AcpCommand::Prompt("hello")
        W->>K: ACP Prompt
        K-->>W: AgentMessageChunk (流式)
        W-->>KA: chunks via UnboundedReceiver
        KA->>KA: 消费所有 chunk 直到 rx 关闭
    end
```

### 8.3 关键设计点

- **独立 worker**：keepalive 使用专用 kiro-cli 进程，不占用业务 worker 队列，不会阻塞用户请求。
- **轻量 prompt**：仅发送 `"hello"` 文本，响应被静默消费丢弃。
- **崩溃恢复**：心跳失败时自动重启 worker，最多连续重试 3 次（带指数退避：5s、10s、15s）。下一个 6 小时周期到来时若仍无 worker 也会再次尝试启动。初始启动失败不影响业务 worker 正常运行。

---

## 9. 初始化时序

```mermaid
sequenceDiagram
    participant M as main
    participant B as AcpBridge::start
    participant T as OS Thread (worker-i)
    participant K as kiro-cli

    M->>B: AcpBridge::start(config)
    loop i in 0..pool_size
        B->>T: std::thread::spawn(acp-worker-i)
        T->>K: Command::new(kiro).spawn()
        T->>T: ClientSideConnection::new(stdin, stdout)
        T->>K: ACP InitializeRequest
        K-->>T: InitializeResponse (capabilities)
        T-->>B: ready_tx.send(Ok(()))
        T->>T: 进入 cmd_rx 等待循环
    end
    B->>B: 等待所有 worker ready_rx（10 秒超时）
    B->>B: spawn_keepalive() — 启动独立保活 worker
    B-->>M: Ok(AcpBridge)
```

每个 worker 初始化完成后通过 `oneshot::Sender<Result<()>>` 发送就绪信号。`AcpBridge::start` 等待所有 worker 就绪（10 秒超时），确保在接受命令前所有 kiro-cli 进程已完成 ACP 握手。随后启动 keepalive worker 进行定期保活（见第 8 节）。

---

## 10. 错误处理

| 场景                             | 处理方式                                                                           |
| -------------------------------- | ---------------------------------------------------------------------------------- |
| worker 线程退出（kiro-cli 崩溃） | `send_cmd` 检测到 send 失败，自动重启 worker 并重试一次                            |
| ACP 初始化失败                   | `ready_tx` 发送错误，`AcpBridge::start` 返回 `Err`，服务启动失败                   |
| ACP 初始化超时                   | `tokio::time::timeout(10s)` 触发，`AcpBridge::start` 返回 `Err`                    |
| Prompt 失败（ACP 层）            | 记录 `tracing::error`，chunk_tx drop，主线程 rx 关闭，最终卡片显示已收到的部分内容 |
| 权限请求无选项                   | 返回 `internal_error`，触发 prompt 失败流程                                        |
| Worker 重启后仍失败              | 返回 `Err` 给调用方，`link.rs` 更新卡片为错误信息                                  |
