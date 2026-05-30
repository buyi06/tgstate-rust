use std::time::Duration;

use reqwest::multipart;
use serde::Serialize;

use crate::constants;
use crate::telegram::types::*;

/// Decide whether a Telegram error response should be retried as rate limiting,
/// and for how many seconds. Returns `None` when the response is not a 429.
///
/// Telegram returns HTTP 429 with `{"error_code":429,"parameters":{"retry_after":N}}`.
/// We trust either the HTTP status or the body's `error_code`, default to 3s when
/// `retry_after` is absent, and cap the wait so a buggy/hostile value cannot stall
/// an upload for minutes.
fn retry_after_secs(status: u16, body: &serde_json::Value) -> Option<u64> {
    let is_rate_limited = status == 429 || body["error_code"].as_i64() == Some(429);
    if !is_rate_limited {
        return None;
    }
    let secs = body
        .get("parameters")
        .and_then(|p| p.get("retry_after"))
        .and_then(|v| v.as_u64())
        .unwrap_or(3)
        .min(60);
    Some(secs)
}

#[derive(Clone)]
pub struct TelegramService {
    pub bot_token: String,
    pub channel_name: String,
    pub client: reqwest::Client,
}

#[derive(Debug, Serialize, Default)]
pub struct DeleteResult {
    pub status: String,
    pub main_file_id: String,
    pub deleted_chunks: Vec<String>,
    pub failed_chunks: Vec<String>,
    pub main_message_deleted: bool,
    pub main_delete_reason: String,
    pub is_manifest: bool,
    pub reason: String,
}

impl TelegramService {
    pub fn new(bot_token: String, channel_name: String, client: reqwest::Client) -> Self {
        Self {
            bot_token,
            channel_name,
            client,
        }
    }

    fn api_url(&self, method: &str) -> String {
        format!("https://api.telegram.org/bot{}/{}", self.bot_token, method)
    }

    fn file_url(&self, file_path: &str) -> String {
        format!(
            "https://api.telegram.org/file/bot{}/{}",
            self.bot_token, file_path
        )
    }

    pub async fn get_download_url(&self, file_id: &str) -> Result<Option<String>, String> {
        let url = self.api_url("getFile");
        let resp = self
            .client
            .post(&url)
            .json(&serde_json::json!({"file_id": file_id}))
            .timeout(std::time::Duration::from_secs(constants::HTTP_TIMEOUT_METADATA_SECS))
            .send()
            .await
            .map_err(|e| format!("getFile request failed: {}", e))?;

        let data: TelegramResponse<TelegramFile> =
            resp.json().await.map_err(|e| format!("Parse error: {}", e))?;

        if data.ok {
            if let Some(file) = data.result {
                if let Some(path) = file.file_path {
                    return Ok(Some(self.file_url(&path)));
                }
            }
        }

        Ok(None)
    }

    /// Upload a document to the channel, retrying on transient network errors
    /// and Telegram 429 rate limiting (honoring `parameters.retry_after`).
    ///
    /// The byte buffer is cloned per attempt; retries are the exception, and a
    /// ~20MB memcpy is negligible next to the network upload it guards. This is
    /// the single entry point used by the (now concurrent) chunked uploader, so
    /// every chunk gets uniform retry/back-off behavior.
    pub async fn send_document_with_retry(
        &self,
        file_bytes: Vec<u8>,
        filename: &str,
        reply_to: Option<i64>,
    ) -> Result<Message, String> {
        let max_attempts = constants::TG_UPLOAD_MAX_RETRIES.max(1);
        let mime_type = mime_guess::from_path(filename)
            .first_or_octet_stream()
            .to_string();

        let mut attempt = 0u32;
        loop {
            attempt += 1;

            let part = multipart::Part::bytes(file_bytes.clone())
                .file_name(filename.to_string())
                .mime_str(&mime_type)
                .map_err(|e| format!("Invalid MIME type: {}", e))?;
            let mut form = multipart::Form::new()
                .text("chat_id", self.channel_name.clone())
                .part("document", part);
            if let Some(reply_id) = reply_to {
                form = form.text("reply_to_message_id", reply_id.to_string());
            }

            let resp = match self
                .client
                .post(&self.api_url("sendDocument"))
                .multipart(form)
                .send()
                .await
            {
                Ok(r) => r,
                Err(e) => {
                    if attempt < max_attempts {
                        tracing::warn!(
                            "sendDocument 网络错误，2s 后重试 ({}/{}): {}",
                            attempt,
                            max_attempts,
                            e
                        );
                        tokio::time::sleep(Duration::from_secs(2)).await;
                        continue;
                    }
                    return Err(format!("sendDocument request failed: {}", e));
                }
            };

            let status = resp.status();
            let data: serde_json::Value = match resp.json().await {
                Ok(v) => v,
                Err(e) => {
                    if attempt < max_attempts {
                        tokio::time::sleep(Duration::from_secs(2)).await;
                        continue;
                    }
                    return Err(format!("Parse error: {}", e));
                }
            };

            if data["ok"].as_bool() == Some(true) {
                let result = data
                    .get("result")
                    .cloned()
                    .unwrap_or(serde_json::Value::Null);
                return serde_json::from_value::<Message>(result)
                    .map_err(|e| format!("Decode message error: {}", e));
            }

            // Retry on Telegram throttling (HTTP 429), honoring retry_after.
            if let Some(wait) = retry_after_secs(status.as_u16(), &data) {
                if attempt < max_attempts {
                    tracing::warn!(
                        "Telegram 限流(429)，{}s 后重试 ({}/{})",
                        wait,
                        attempt,
                        max_attempts
                    );
                    tokio::time::sleep(Duration::from_secs(wait)).await;
                    continue;
                }
            }

            let desc = data["description"].as_str().unwrap_or("unknown error");
            return Err(format!("sendDocument error: {}", desc));
        }
    }

    pub async fn delete_message(&self, message_id: i64) -> (bool, String) {
        let url = self.api_url("deleteMessage");
        match self
            .client
            .post(&url)
            .json(&serde_json::json!({
                "chat_id": self.channel_name,
                "message_id": message_id
            }))
            .timeout(std::time::Duration::from_secs(constants::HTTP_TIMEOUT_METADATA_SECS))
            .send()
            .await
        {
            Ok(resp) => {
                let data: serde_json::Value = resp.json().await.unwrap_or_default();
                if data["ok"].as_bool() == Some(true) {
                    (true, "deleted".into())
                } else {
                    let desc = data["description"].as_str().unwrap_or("");
                    if desc.contains("not found") {
                        (true, "not_found".into())
                    } else {
                        (false, "error".into())
                    }
                }
            }
            Err(e) => {
                tracing::error!("deleteMessage failed: {}", e);
                (false, "error".into())
            }
        }
    }

    pub async fn delete_file_with_chunks(&self, file_id: &str) -> DeleteResult {
        let mut result = DeleteResult {
            main_file_id: file_id.to_string(),
            ..Default::default()
        };

        // Parse composite ID
        let parts: Vec<&str> = file_id.splitn(2, ':').collect();
        if parts.len() != 2 {
            result.status = "error".into();
            result.reason = "Invalid file_id format".into();
            return result;
        }

        let message_id: i64 = match parts[0].parse() {
            Ok(id) => id,
            Err(_) => {
                result.status = "error".into();
                result.reason = "Invalid message_id".into();
                return result;
            }
        };
        let actual_file_id = parts[1];

        // Check if manifest
        if let Ok(Some(url)) = self.get_download_url(actual_file_id).await {
            if let Ok(resp) = self.client.get(&url).send().await {
                if let Ok(body) = resp.bytes().await {
                    if body.starts_with(b"tgstate-blob\n") {
                        result.is_manifest = true;
                        let content = String::from_utf8_lossy(&body);
                        let lines: Vec<&str> = content.lines().collect();

                        if lines.len() >= 3 {
                            let chunk_ids: Vec<String> =
                                lines[2..].iter().map(|s| s.to_string()).collect();

                            // Concurrent delete with semaphore
                            let sem = std::sync::Arc::new(tokio::sync::Semaphore::new(10));
                            let mut handles = Vec::new();

                            for cid in chunk_ids {
                                let sem = sem.clone();
                                let tg = self.clone();
                                handles.push(tokio::spawn(async move {
                                    let _permit = sem.acquire().await;
                                    let parts: Vec<&str> = cid.splitn(2, ':').collect();
                                    if parts.len() == 2 {
                                        if let Ok(mid) = parts[0].parse::<i64>() {
                                            let (ok, _) = tg.delete_message(mid).await;
                                            return (cid, ok);
                                        }
                                    }
                                    (cid, false)
                                }));
                            }

                            for handle in handles {
                                if let Ok((cid, ok)) = handle.await {
                                    if ok {
                                        result.deleted_chunks.push(cid);
                                    } else {
                                        result.failed_chunks.push(cid);
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }

        // Delete main message
        let (main_ok, reason) = self.delete_message(message_id).await;
        result.main_message_deleted = main_ok;
        result.main_delete_reason = reason;

        result.status = if main_ok && result.failed_chunks.is_empty() {
            "success".into()
        } else {
            "partial_failure".into()
        };

        result
    }

    pub async fn try_get_manifest_original_filename(
        &self,
        manifest_file_id: &str,
    ) -> Result<String, String> {
        let url = self
            .get_download_url(manifest_file_id)
            .await?
            .ok_or("No download URL")?;

        let resp = self
            .client
            .get(&url)
            .timeout(std::time::Duration::from_secs(constants::HTTP_TIMEOUT_METADATA_SECS))
            .send()
            .await
            .map_err(|e| format!("Download manifest failed: {}", e))?;

        let body = resp.bytes().await.map_err(|e| e.to_string())?;

        if !body.starts_with(b"tgstate-blob\n") {
            return Err("Not a manifest file".into());
        }

        let content = String::from_utf8_lossy(&body);
        let lines: Vec<&str> = content.lines().collect();
        if lines.len() < 2 {
            return Err("Invalid manifest format".into());
        }

        Ok(lines[1].to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::retry_after_secs;
    use serde_json::json;

    #[test]
    fn retry_after_uses_telegram_parameter() {
        let body = json!({
            "ok": false,
            "error_code": 429,
            "description": "Too Many Requests: retry after 5",
            "parameters": { "retry_after": 5 }
        });
        assert_eq!(retry_after_secs(429, &body), Some(5));
    }

    #[test]
    fn retry_after_defaults_to_3s_when_missing_but_rate_limited() {
        let body = json!({ "ok": false, "error_code": 429, "description": "Too Many Requests" });
        assert_eq!(retry_after_secs(429, &body), Some(3));
    }

    #[test]
    fn retry_after_detects_429_from_body_even_when_status_differs() {
        // A proxy may rewrite the HTTP status; the JSON error_code is enough.
        let body = json!({ "ok": false, "error_code": 429, "parameters": { "retry_after": 10 } });
        assert_eq!(retry_after_secs(200, &body), Some(10));
    }

    #[test]
    fn retry_after_is_capped_at_60s() {
        let body = json!({ "ok": false, "error_code": 429, "parameters": { "retry_after": 120 } });
        assert_eq!(retry_after_secs(429, &body), Some(60));
    }

    #[test]
    fn retry_after_none_for_non_rate_limited_errors() {
        let body = json!({ "ok": false, "error_code": 400, "description": "Bad Request: chat not found" });
        assert_eq!(retry_after_secs(400, &body), None);
    }
}
