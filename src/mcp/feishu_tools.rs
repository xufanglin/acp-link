//! 飞书相关的 MCP Tool 定义与实现

use serde_json::{Value, json};

use crate::feishu::FeishuClient;

/// 返回飞书相关 tools 的 schema 列表
pub fn list() -> Vec<Value> {
    vec![json!({
        "name": "feishu_send_file",
        "description": "Upload and send a file to the current Feishu chat thread. For image files (.png/.jpg/.gif/.webp/.bmp), sent as inline image; otherwise as file attachment. Extract message_id from [feishu_context] in the conversation.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "file_path": {
                    "type": "string",
                    "description": "Absolute path to the file to upload"
                },
                "message_id": {
                    "type": "string",
                    "description": "The Feishu message_id to reply to (from feishu_context)"
                },
                "file_name": {
                    "type": "string",
                    "description": "Optional display name for the file. Defaults to the basename of file_path"
                }
            },
            "required": ["file_path", "message_id"]
        }
    })]
}

/// 分发飞书 tool 调用，返回 JSON-RPC result content
pub async fn call(tool_name: &str, args: &Value, client: &FeishuClient) -> Result<Value, String> {
    match tool_name {
        "feishu_send_file" => send_file(args, client).await,
        _ => Err(format!("unknown tool: {tool_name}")),
    }
}

/// 上传并发送文件到飞书会话（图片走 inline，其他走文件附件）
async fn send_file(args: &Value, client: &FeishuClient) -> Result<Value, String> {
    let file_path = args.get("file_path").and_then(|v| v.as_str()).unwrap_or("");
    let message_id = args
        .get("message_id")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let file_name = args
        .get("file_name")
        .and_then(|v| v.as_str())
        .map(str::to_string)
        .unwrap_or_else(|| basename(file_path).to_string());

    tracing::debug!(
        "feishu_send_file 调用: file_path={file_path}, message_id={message_id}, file_name={file_name}"
    );

    if file_path.is_empty() || message_id.is_empty() {
        tracing::warn!("feishu_send_file 参数缺失: file_path={file_path:?}, message_id={message_id:?}");
        return Err("file_path and message_id are required".into());
    }

    let data = std::fs::read(file_path).map_err(|e| {
        tracing::error!("feishu_send_file 读取文件失败: {file_path} -> {e}");
        format!("读取文件失败: {e}")
    })?;
    tracing::debug!("feishu_send_file 文件已读取: {} bytes, is_image={}", data.len(), is_image_file(file_path));

    if is_image_file(file_path) {
        let image_key = client.upload_image(&file_name, &data).await.map_err(|e| {
            tracing::error!("feishu_send_file 上传图片失败: {e:#}");
            format!("{e:#}")
        })?;
        tracing::debug!("feishu_send_file 图片已上传: image_key={image_key}");
        client
            .send_image_reply(message_id, &image_key)
            .await
            .map_err(|e| {
                tracing::error!("feishu_send_file 发送图片回复失败: message_id={message_id}, {e:#}");
                format!("{e:#}")
            })?;
        tracing::info!("feishu_send_file 图片发送成功: message_id={message_id}, image_key={image_key}");
        Ok(json!({ "status": "sent", "type": "image", "image_key": image_key }))
    } else {
        let file_key = client
            .upload_file(&file_name, &data)
            .await
            .map_err(|e| {
                tracing::error!("feishu_send_file 上传文件失败: {e:#}");
                format!("{e:#}")
            })?;
        tracing::debug!("feishu_send_file 文件已上传: file_key={file_key}");
        client
            .send_file_reply(message_id, &file_key)
            .await
            .map_err(|e| {
                tracing::error!("feishu_send_file 发送文件回复失败: message_id={message_id}, {e:#}");
                format!("{e:#}")
            })?;
        tracing::info!("feishu_send_file 文件发送成功: message_id={message_id}, file_key={file_key}, file_name={file_name}");
        Ok(json!({ "status": "sent", "type": "file", "file_key": file_key, "file_name": file_name }))
    }
}

/// 根据文件扩展名判断是否为图片文件
fn is_image_file(path: &str) -> bool {
    let lower = path.to_lowercase();
    lower.ends_with(".png")
        || lower.ends_with(".jpg")
        || lower.ends_with(".jpeg")
        || lower.ends_with(".gif")
        || lower.ends_with(".webp")
        || lower.ends_with(".bmp")
}

/// 提取路径的文件名部分，空路径回退为 `"file"`
fn basename(path: &str) -> &str {
    std::path::Path::new(path)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("file")
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── is_image_file ──────────────────────────────────────────────────────

    #[test]
    fn test_is_image_file_png() {
        assert!(is_image_file("photo.png"));
        assert!(is_image_file("/tmp/photo.PNG"));
    }

    #[test]
    fn test_is_image_file_jpg_jpeg() {
        assert!(is_image_file("photo.jpg"));
        assert!(is_image_file("photo.jpeg"));
        assert!(is_image_file("photo.JPEG"));
    }

    #[test]
    fn test_is_image_file_gif_webp_bmp() {
        assert!(is_image_file("anim.gif"));
        assert!(is_image_file("modern.webp"));
        assert!(is_image_file("legacy.bmp"));
    }

    #[test]
    fn test_is_image_file_non_image() {
        assert!(!is_image_file("report.pdf"));
        assert!(!is_image_file("data.json"));
        assert!(!is_image_file("archive.zip"));
        assert!(!is_image_file("no_ext"));
    }

    // ── basename ────────────────────────────────────────────────────────────

    #[test]
    fn test_basename_normal_path() {
        assert_eq!(basename("/tmp/report.pdf"), "report.pdf");
    }

    #[test]
    fn test_basename_filename_only() {
        assert_eq!(basename("file.txt"), "file.txt");
    }

    #[test]
    fn test_basename_empty_string() {
        // 空路径回退到 "file"
        assert_eq!(basename(""), "file");
    }

    #[test]
    fn test_basename_nested_path() {
        assert_eq!(basename("/a/b/c/deep.png"), "deep.png");
    }

    // ── list ────────────────────────────────────────────────────────────────

    #[test]
    fn test_list_returns_feishu_send_file_tool() {
        let tools = list();
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0]["name"], "feishu_send_file");
        assert!(tools[0]["inputSchema"]["required"].as_array().unwrap().len() >= 2);
    }

    // ── call 未知工具 ──────────────────────────────────────────────────────

    #[tokio::test]
    async fn test_call_unknown_tool_returns_error() {
        let client = FeishuClient::new("id", "secret");
        let result = call("nonexistent_tool", &json!({}), &client).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("unknown tool"));
    }
}
