use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// topic_id 与 ACP session_id 的映射，持久化到磁盘
///
/// 同时维护 message_id → topic_id 的反向映射，用于从 root_id 查找 topic_id。
pub struct SessionMap {
    path: PathBuf,
    /// topic_id -> SessionEntry
    entries: HashMap<String, SessionEntry>,
    /// message_id -> topic_id（从 entries 构建的反向索引）
    topic_index: HashMap<String, String>,
}

#[derive(Clone, Serialize, Deserialize)]
struct SessionEntry {
    session_id: String,
    /// 触发创建 topic 的原始 message_id
    message_id: String,
    /// 创建时间（Unix 秒）
    created_at: u64,
}

#[derive(Serialize, Deserialize)]
struct SessionMapFile {
    sessions: HashMap<String, SessionEntry>,
}

/// 当前 Unix 时间戳（秒）
fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

impl SessionMap {
    /// 加载或创建 session 映射文件
    ///
    /// # Arguments
    ///
    /// * `path` - JSON 文件路径（如 `~/.acp-link/sessions.json`）
    pub fn load(path: &Path) -> Result<Self> {
        let entries = if path.exists() {
            let content = std::fs::read_to_string(path)
                .with_context(|| format!("读取 session 映射失败: {}", path.display()))?;
            let file: SessionMapFile = serde_json::from_str(&content)
                .with_context(|| format!("解析 session 映射失败: {}", path.display()))?;
            file.sessions
        } else {
            HashMap::new()
        };

        let topic_index = Self::build_topic_index(&entries);

        tracing::info!(
            "Session 映射已加载: {} 条记录, {} 条 topic 索引",
            entries.len(),
            topic_index.len()
        );

        Ok(Self {
            path: path.to_owned(),
            entries,
            topic_index,
        })
    }

    /// 查找 topic_id 对应的 session_id（空字符串视为无 session）
    pub fn get_session_id(&self, topic_id: &str) -> Option<&str> {
        self.entries
            .get(topic_id)
            .map(|e| e.session_id.as_str())
            .filter(|s| !s.is_empty())
    }

    /// 通过 message_id（root_id）查找 topic_id
    pub fn get_topic_id(&self, message_id: &str) -> Option<&str> {
        self.topic_index.get(message_id).map(|s| s.as_str())
    }

    /// 记录 message_id → topic_id 映射（创建 topic 时调用，尚无 session）
    pub fn map_topic(&mut self, message_id: &str, topic_id: &str) -> Result<()> {
        self.topic_index
            .insert(message_id.to_owned(), topic_id.to_owned());

        // 如果还没有对应的 session entry，创建一个占位的（session_id 为空）
        self.entries
            .entry(topic_id.to_owned())
            .or_insert_with(|| SessionEntry {
                session_id: String::new(),
                message_id: message_id.to_owned(),
                created_at: now_secs(),
            });
        self.flush()
    }

    /// 插入/更新 session_id 并持久化到磁盘
    pub fn insert(&mut self, topic_id: &str, session_id: &str) -> Result<()> {
        if let Some(entry) = self.entries.get_mut(topic_id) {
            entry.session_id = session_id.to_owned();
        } else {
            self.entries.insert(
                topic_id.to_owned(),
                SessionEntry {
                    session_id: session_id.to_owned(),
                    message_id: String::new(),
                    created_at: now_secs(),
                },
            );
        }
        self.flush()
    }

    /// 清理过期 session，返回清理数量
    ///
    /// # Arguments
    ///
    /// * `retention` - 保留天数
    pub fn cleanup_expired(&mut self, retention: u32) -> Result<usize> {
        let ttl_secs = u64::from(retention) * 24 * 3600;
        let cutoff = now_secs().saturating_sub(ttl_secs);
        let before = self.entries.len();
        self.entries.retain(|_, e| e.created_at > cutoff);
        let removed = before - self.entries.len();

        if removed > 0 {
            self.topic_index = Self::build_topic_index(&self.entries);
            tracing::info!("Session 清理: 移除 {removed} 条过期记录");
            self.flush()?;
        }

        Ok(removed)
    }

    /// 从 entries 构建 message_id → topic_id 反向索引
    fn build_topic_index(entries: &HashMap<String, SessionEntry>) -> HashMap<String, String> {
        entries
            .iter()
            .filter(|(_, e)| !e.message_id.is_empty())
            .map(|(topic_id, e)| (e.message_id.clone(), topic_id.clone()))
            .collect()
    }

    /// 返回当前 entry 数量（供测试使用）
    #[cfg(test)]
    fn len(&self) -> usize {
        self.entries.len()
    }

    /// 将映射原子性地写入磁盘（先写临时文件，再 rename，防止崩溃导致文件损坏）
    pub(crate) fn flush(&self) -> Result<()> {
        let file = SessionMapFile {
            sessions: self.entries.clone(),
        };
        let content = serde_json::to_string_pretty(&file).context("序列化 session 映射失败")?;

        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("创建目录失败: {}", parent.display()))?;
        }

        let tmp_path = self.path.with_file_name(format!(
            "{}.tmp",
            self.path.file_name().unwrap_or_default().to_string_lossy()
        ));
        std::fs::write(&tmp_path, &content)
            .with_context(|| format!("写入临时 session 文件失败: {}", tmp_path.display()))?;
        std::fs::rename(&tmp_path, &self.path).with_context(|| {
            format!(
                "重命名 session 文件失败: {} -> {}",
                tmp_path.display(),
                self.path.display()
            )
        })?;

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 在临时目录中创建 SessionMap，返回 (目录路径, SessionMap)
    fn make_session_map() -> (PathBuf, SessionMap) {
        let dir = std::env::temp_dir().join(format!(
            "acp_session_test_{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("sessions.json");
        let map = SessionMap::load(&path).unwrap();
        (dir, map)
    }

    #[test]
    fn test_load_creates_empty_map_when_file_missing() {
        // 文件不存在时应创建空映射
        let (dir, map) = make_session_map();
        assert_eq!(map.len(), 0);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_insert_and_get_session_id() {
        // insert 后 get_session_id 应返回对应值
        let (dir, mut map) = make_session_map();

        map.insert("thread_001", "session_abc").unwrap();
        assert_eq!(map.get_session_id("thread_001"), Some("session_abc"));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_get_session_id_returns_none_for_unknown_thread() {
        // 未插入的 topic_id 应返回 None
        let (dir, map) = make_session_map();
        assert!(map.get_session_id("nonexistent").is_none());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_get_session_id_ignores_empty_session_id() {
        // session_id 为空字符串时 get_session_id 应返回 None
        let (dir, mut map) = make_session_map();
        // map_topic 创建占位 entry，session_id 为空
        map.map_topic("msg_001", "thread_001").unwrap();
        assert!(map.get_session_id("thread_001").is_none());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_map_topic_and_get_topic_id() {
        // map_topic 后通过 message_id 应能查到 topic_id
        let (dir, mut map) = make_session_map();

        map.map_topic("msg_001", "thread_001").unwrap();
        assert_eq!(map.get_topic_id("msg_001"), Some("thread_001"));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_get_topic_id_returns_none_for_unknown_message() {
        // 未记录的 message_id 应返回 None
        let (dir, map) = make_session_map();
        assert!(map.get_topic_id("no_such_msg").is_none());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_insert_updates_existing_session_id() {
        // 对同一 topic 多次 insert，session_id 应被更新
        let (dir, mut map) = make_session_map();

        map.insert("thread_001", "session_v1").unwrap();
        map.insert("thread_001", "session_v2").unwrap();
        assert_eq!(map.get_session_id("thread_001"), Some("session_v2"));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_map_topic_then_insert_keeps_topic_index() {
        // map_topic 后再 insert session_id，topic_index 中的映射应仍有效
        let (dir, mut map) = make_session_map();

        map.map_topic("msg_001", "thread_001").unwrap();
        map.insert("thread_001", "session_abc").unwrap();

        assert_eq!(map.get_topic_id("msg_001"), Some("thread_001"));
        assert_eq!(map.get_session_id("thread_001"), Some("session_abc"));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_persistence_across_reload() {
        // flush 后重新 load 应能读到之前写入的数据
        let dir = std::env::temp_dir().join(format!(
            "acp_session_persist_{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("sessions.json");

        {
            let mut map = SessionMap::load(&path).unwrap();
            map.map_topic("msg_x", "thread_x").unwrap();
            map.insert("thread_x", "session_x").unwrap();
        }

        // 重新加载
        let map2 = SessionMap::load(&path).unwrap();
        assert_eq!(map2.get_session_id("thread_x"), Some("session_x"));
        assert_eq!(map2.get_topic_id("msg_x"), Some("thread_x"));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_cleanup_expired_removes_old_entries() {
        // 手动写入一个 created_at=0（过期）的 session 文件，cleanup 应将其移除
        let dir = std::env::temp_dir().join(format!(
            "acp_session_cleanup_{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("sessions.json");

        // 写入一个极早 created_at 的 entry（肯定过期）
        let old_content = serde_json::json!({
            "sessions": {
                "thread_old": {
                    "session_id": "session_old",
                    "message_id": "msg_old",
                    "created_at": 1  // Unix 1 秒 = 1970年，必然过期
                },
                "thread_new": {
                    "session_id": "session_new",
                    "message_id": "msg_new",
                    "created_at": now_secs()  // 当前时间，未过期
                }
            }
        });
        std::fs::write(&path, old_content.to_string()).unwrap();

        let mut map = SessionMap::load(&path).unwrap();
        assert_eq!(map.len(), 2);

        let removed = map.cleanup_expired(3).unwrap();
        assert_eq!(removed, 1);
        assert_eq!(map.len(), 1);
        assert!(map.get_session_id("thread_old").is_none());
        assert_eq!(map.get_session_id("thread_new"), Some("session_new"));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_cleanup_expired_returns_zero_when_nothing_expired() {
        // 全部 entry 未过期时 cleanup 应返回 0
        let (dir, mut map) = make_session_map();
        map.insert("thread_001", "session_abc").unwrap();

        let removed = map.cleanup_expired(3).unwrap();
        assert_eq!(removed, 0);

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_load_invalid_json_returns_error() {
        // sessions.json 内容非法时应返回错误
        let dir = std::env::temp_dir().join(format!(
            "acp_session_err_{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("sessions.json");
        std::fs::write(&path, "not json at all").unwrap();

        let result = SessionMap::load(&path);
        assert!(result.is_err());

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_cleanup_expired_also_clears_topic_index() {
        // cleanup_expired 移除过期 entry 后，topic_index 也应同步清除
        let dir = std::env::temp_dir().join(format!(
            "acp_session_idx_{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("sessions.json");

        let old_content = serde_json::json!({
            "sessions": {
                "thread_old": {
                    "session_id": "session_old",
                    "message_id": "msg_old",
                    "created_at": 1
                }
            }
        });
        std::fs::write(&path, old_content.to_string()).unwrap();

        let mut map = SessionMap::load(&path).unwrap();
        // 过期前 topic_index 应能查到
        assert_eq!(map.get_topic_id("msg_old"), Some("thread_old"));

        let removed = map.cleanup_expired(3).unwrap();
        assert_eq!(removed, 1);
        // cleanup 后 topic_index 也应移除对应记录
        assert!(map.get_topic_id("msg_old").is_none());

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_map_topic_does_not_overwrite_existing_entry() {
        // 对同一 topic_id 重复调用 map_topic，不应覆盖已有 entry 的 session_id
        let (dir, mut map) = make_session_map();

        map.map_topic("msg_001", "thread_001").unwrap();
        map.insert("thread_001", "session_abc").unwrap();

        // 再次 map_topic，entry 的 session_id 应保持不变
        map.map_topic("msg_001", "thread_001").unwrap();
        assert_eq!(map.get_session_id("thread_001"), Some("session_abc"));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_insert_without_map_topic_has_no_topic_index() {
        // 直接 insert（不经过 map_topic）时，topic_index 中不应有任何 message_id 条目
        let (dir, mut map) = make_session_map();

        map.insert("thread_001", "session_abc").unwrap();
        // session_id 可查到
        assert_eq!(map.get_session_id("thread_001"), Some("session_abc"));
        // 但没有 message_id，任何 get_topic_id 查询都应为 None
        assert!(map.get_topic_id("thread_001").is_none());

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_multiple_threads_independent() {
        // 多个 topic 互不干扰
        let (dir, mut map) = make_session_map();

        map.map_topic("msg_a", "thread_a").unwrap();
        map.map_topic("msg_b", "thread_b").unwrap();
        map.insert("thread_a", "session_a").unwrap();
        map.insert("thread_b", "session_b").unwrap();

        assert_eq!(map.get_session_id("thread_a"), Some("session_a"));
        assert_eq!(map.get_session_id("thread_b"), Some("session_b"));
        assert_eq!(map.get_topic_id("msg_a"), Some("thread_a"));
        assert_eq!(map.get_topic_id("msg_b"), Some("thread_b"));
        assert_eq!(map.len(), 2);

        std::fs::remove_dir_all(&dir).ok();
    }
}
