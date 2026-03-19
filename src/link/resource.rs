use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use anyhow::{Context, Result};
use sha2::{Digest, Sha256};

use crate::im::IMChannel;

/// 资源存储：将飞书下载的图片/文件以 SHA256 去重保存到本地目录
#[derive(Clone)]
pub struct ResourceStore {
    save_dir: PathBuf,
}

impl ResourceStore {
    /// 创建资源存储
    ///
    /// # Arguments
    ///
    /// * `save_dir` - 文件保存目录，必须已存在
    pub fn new(save_dir: &Path) -> Self {
        Self {
            save_dir: save_dir.to_owned(),
        }
    }

    /// 从 IM 平台下载资源并存储，返回本地路径
    ///
    /// 文件以 `{sha256}.{ext}` 命名，已存在则跳过下载。
    ///
    /// # Arguments
    ///
    /// * `channel` - IM channel（实现 IMChannel trait）
    /// * `message_id` - 消息 ID
    /// * `file_key` - 资源 key
    /// * `resource_type` - `"file"` 或 `"image"`
    /// * `original_name` - 原始文件名（用于提取扩展名）
    pub async fn save_resource(
        &self,
        channel: &dyn IMChannel,
        message_id: &str,
        file_key: &str,
        resource_type: &str,
        original_name: &str,
    ) -> Result<PathBuf> {
        let data = channel
            .download_resource(message_id, file_key, resource_type)
            .await
            .with_context(|| format!("下载资源失败: {file_key}"))?;

        let hash = hex_sha256(&data);
        let ext = Path::new(original_name)
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("bin");
        let filename = format!("{hash}.{ext}");
        let path = self.save_dir.join(&filename);

        if !path.exists() {
            tokio::fs::write(&path, &data)
                .await
                .with_context(|| format!("写入资源文件失败: {}", path.display()))?;
            tracing::info!("资源已保存: {filename} ({} bytes)", data.len());
        } else {
            // 刷新 mtime，防止去重文件被清理误删
            let times = std::fs::FileTimes::new().set_modified(SystemTime::now());
            if let Err(e) = std::fs::File::open(&path).and_then(|f| f.set_times(times)) {
                tracing::warn!("刷新资源 mtime 失败: {filename} - {e}");
            }
            tracing::debug!("资源已存在，跳过: {filename}");
        }

        Ok(path)
    }

    /// 清理过期资源文件，返回清理数量
    ///
    /// # Arguments
    ///
    /// * `retention` - 保留天数
    pub fn cleanup_expired(&self, retention: u32) -> Result<usize> {
        let resource_ttl = Duration::from_secs(u64::from(retention) * 24 * 3600);
        let dir = match std::fs::read_dir(&self.save_dir) {
            Ok(d) => d,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(0),
            Err(e) => {
                return Err(anyhow::anyhow!(e)
                    .context(format!("读取目录失败: {}", self.save_dir.display())));
            }
        };

        let now = SystemTime::now();
        let mut removed = 0;

        for entry in dir.flatten() {
            let path = entry.path();
            if !path.is_file() {
                continue;
            }
            let modified = match entry.metadata().and_then(|m| m.modified()) {
                Ok(t) => t,
                Err(_) => continue,
            };
            if now.duration_since(modified).unwrap_or_default() > resource_ttl {
                if let Err(e) = std::fs::remove_file(&path) {
                    tracing::warn!("删除过期资源失败: {} - {e}", path.display());
                } else {
                    removed += 1;
                }
            }
        }

        if removed > 0 {
            tracing::info!("资源清理: 删除 {removed} 个过期文件");
        }

        Ok(removed)
    }

    /// 将本地路径转为 `file:///` URI
    pub fn to_file_uri(path: &Path) -> String {
        format!("file://{}", path.display())
    }
}

/// 计算数据的 SHA256 哈希并返回十六进制字符串
fn hex_sha256(data: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(data);
    format!("{:x}", hasher.finalize())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 创建唯一临时目录
    fn make_temp_dir() -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "acp_resource_test_{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn test_hex_sha256_known_value() {
        // "hello" 的 SHA256 已知值
        let hash = hex_sha256(b"hello");
        assert_eq!(
            hash,
            "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824"
        );
    }

    #[test]
    fn test_hex_sha256_empty_input() {
        // 空输入的 SHA256 已知值
        let hash = hex_sha256(b"");
        assert_eq!(
            hash,
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }

    #[test]
    fn test_hex_sha256_deterministic() {
        // 相同输入应产生相同哈希
        assert_eq!(hex_sha256(b"data"), hex_sha256(b"data"));
    }

    #[test]
    fn test_hex_sha256_different_inputs_differ() {
        // 不同输入应产生不同哈希
        assert_ne!(hex_sha256(b"abc"), hex_sha256(b"def"));
    }

    #[test]
    fn test_to_file_uri_absolute_path() {
        let path = Path::new("/tmp/abc123.png");
        assert_eq!(ResourceStore::to_file_uri(path), "file:///tmp/abc123.png");
    }

    #[test]
    fn test_to_file_uri_nested_path() {
        let path = Path::new("/home/user/.acp-link/data/deadbeef.pdf");
        assert_eq!(
            ResourceStore::to_file_uri(path),
            "file:///home/user/.acp-link/data/deadbeef.pdf"
        );
    }

    #[test]
    fn test_cleanup_expired_returns_zero_for_empty_dir() {
        // 空目录，清理应返回 0
        let dir = make_temp_dir();
        let store = ResourceStore::new(&dir);
        let removed = store.cleanup_expired(3).unwrap();
        assert_eq!(removed, 0);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_cleanup_expired_ignores_fresh_files() {
        // 刚创建的文件未过期，不应被删除
        let dir = make_temp_dir();
        let file_path = dir.join("fresh.bin");
        std::fs::write(&file_path, b"content").unwrap();

        let store = ResourceStore::new(&dir);
        let removed = store.cleanup_expired(3).unwrap();
        assert_eq!(removed, 0);
        assert!(file_path.exists());

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_cleanup_expired_removes_old_files() {
        // 将文件 mtime 设置到远早于 3 天前，清理应将其删除
        let dir = make_temp_dir();
        let file_path = dir.join("old_file.bin");
        std::fs::write(&file_path, b"old content").unwrap();

        // 通过 touch -t 将修改时间设置为 1970 年（肯定过期）
        let status = std::process::Command::new("touch")
            .args(["-t", "197001010000", file_path.to_str().unwrap()])
            .status()
            .expect("touch 命令执行失败");
        assert!(status.success(), "touch -t 应成功");

        let store = ResourceStore::new(&dir);
        let removed = store.cleanup_expired(3).unwrap();
        assert_eq!(removed, 1);
        assert!(!file_path.exists());

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_cleanup_expired_returns_zero_for_nonexistent_dir() {
        // 目录不存在时应返回 0（而非错误）
        let store = ResourceStore::new(Path::new("/nonexistent/path/that/does/not/exist"));
        let removed = store.cleanup_expired(3).unwrap();
        assert_eq!(removed, 0);
    }

    #[test]
    fn test_resource_store_new() {
        // ResourceStore::new 不依赖目录存在
        let _store = ResourceStore::new(Path::new("/some/path"));
        // 仅验证 to_file_uri 可正常调用
        let uri = ResourceStore::to_file_uri(Path::new("/some/path/file.png"));
        assert!(uri.starts_with("file://"));
    }

    #[test]
    fn test_hex_sha256_output_format() {
        // SHA256 输出应为 64 个小写十六进制字符
        let hash = hex_sha256(b"test data");
        assert_eq!(hash.len(), 64);
        assert!(hash.chars().all(|c| c.is_ascii_hexdigit()));
        // 确保全部小写（无大写字母）
        assert_eq!(hash, hash.to_lowercase());
    }

    #[test]
    fn test_cleanup_expired_ignores_subdirectories() {
        // 子目录不应被 cleanup 删除（is_file() 分支）
        let dir = make_temp_dir();
        let sub_dir = dir.join("subdir");
        std::fs::create_dir_all(&sub_dir).unwrap();

        let store = ResourceStore::new(&dir);
        let removed = store.cleanup_expired(3).unwrap();
        // 子目录不是文件，不应被计入删除数量
        assert_eq!(removed, 0);
        assert!(sub_dir.exists(), "子目录不应被删除");

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_cleanup_expired_mixed_files() {
        // 同时包含过期和新鲜文件：只有过期文件被删除
        let dir = make_temp_dir();

        let old_file = dir.join("old.bin");
        let fresh_file = dir.join("fresh.bin");
        std::fs::write(&old_file, b"old").unwrap();
        std::fs::write(&fresh_file, b"fresh").unwrap();

        // 将 old_file 的 mtime 设为过期
        let status = std::process::Command::new("touch")
            .args(["-t", "197001010000", old_file.to_str().unwrap()])
            .status()
            .expect("touch 命令执行失败");
        assert!(status.success());

        let store = ResourceStore::new(&dir);
        let removed = store.cleanup_expired(3).unwrap();
        assert_eq!(removed, 1);
        assert!(!old_file.exists(), "过期文件应被删除");
        assert!(fresh_file.exists(), "新鲜文件不应被删除");

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_cleanup_expired_multiple_old_files() {
        // 多个过期文件应全部被删除
        let dir = make_temp_dir();

        let files: Vec<_> = (0..3)
            .map(|i| {
                let p = dir.join(format!("old_{i}.bin"));
                std::fs::write(&p, b"data").unwrap();
                p
            })
            .collect();

        for path in &files {
            let status = std::process::Command::new("touch")
                .args(["-t", "197001010000", path.to_str().unwrap()])
                .status()
                .expect("touch 命令执行失败");
            assert!(status.success());
        }

        let store = ResourceStore::new(&dir);
        let removed = store.cleanup_expired(3).unwrap();
        assert_eq!(removed, 3);
        for path in &files {
            assert!(!path.exists(), "过期文件 {:?} 应被删除", path);
        }

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_resource_store_clone() {
        // Clone 后两个实例应独立操作同一目录
        let dir = make_temp_dir();
        let store1 = ResourceStore::new(&dir);
        let store2 = store1.clone();

        // 两个实例均能正常执行 cleanup
        assert_eq!(store1.cleanup_expired(3).unwrap(), 0);
        assert_eq!(store2.cleanup_expired(3).unwrap(), 0);

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_to_file_uri_filename_only() {
        // 验证带扩展名的文件路径
        let path = Path::new("/tmp/deadbeef1234.png");
        let uri = ResourceStore::to_file_uri(path);
        assert!(uri.ends_with(".png"));
        assert!(uri.starts_with("file:///tmp/"));
    }
}
