use reqwest::multipart;
use serde::Serialize;

use crate::database;
use crate::telegram::types::*;

const CHUNK_SIZE_BYTES: u64 = (19.5 * 1024.0 * 1024.0) as u64;

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

    async fn send_document(
        &self,
        file_bytes: Vec<u8>,
        filename: &str,
        reply_to: Option<i64>,
    ) -> Result<Message, String> {
        let form = multipart::Form::new()
            .text("chat_id", self.channel_name.clone())
            .part(
                "document",
                multipart::Part::bytes(file_bytes)
                    .file_name(filename.to_string()),
            );

        let form = if let Some(reply_id) = reply_to {
            form.text("reply_to_message_id", reply_id.to_string())
        } else {
            form
        };

        let resp = self
            .client
            .post(&self.api_url("sendDocument"))
            .multipart(form)
            .send()
            .await
            .map_err(|e| format!("sendDocument failed: {}", e))?;

        let data: TelegramResponse<Message> = resp
            .json()
            .await
            .map_err(|e| format!("Parse sendDocument response: {}", e))?;

        if data.ok {
            data.result.ok_or_else(|| "No result in response".into())
        } else {
            Err(format!(
                "sendDocument error: {}",
                data.description.unwrap_or_default()
            ))
        }
    }

    pub async fn upload_file(
        &self,
        file_path: &str,
        file_name: &str,
        db_path: &str,
    ) -> Result<String, String> {
        if self.channel_name.is_empty() {
            return Err("CHANNEL_NAME not configured".into());
        }

        let metadata = std::fs::metadata(file_path).map_err(|e| e.to_string())?;
        let file_size = metadata.len();

        if file_size >= CHUNK_SIZE_BYTES {
            tracing::info!("文件 {} 较大 ({}MB)，将分块上传", file_name, file_size / (1024 * 1024));
            return self.upload_as_chunks(file_path, file_name, db_path).await;
        }

        tracing::info!("直接上传文件: {}", file_name);
        let file_bytes = std::fs::read(file_path).map_err(|e| e.to_string())?;
        let message = self.send_document(file_bytes, file_name, None).await?;

        let doc = message
            .document
            .ok_or("No document in response")?;
        let composite_id = format!("{}:{}", message.message_id, doc.file_id);

        let short_id = database::add_file_metadata(db_path, file_name, &composite_id, file_size as i64)?;
        Ok(short_id)
    }

    async fn upload_as_chunks(
        &self,
        file_path: &str,
        original_filename: &str,
        db_path: &str,
    ) -> Result<String, String> {
        let file_bytes = std::fs::read(file_path).map_err(|e| e.to_string())?;
        let total_size = file_bytes.len() as i64;
        let mut chunk_ids: Vec<String> = Vec::new();
        let mut first_message_id: Option<i64> = None;
        let mut chunk_num = 0;

        for chunk in file_bytes.chunks(CHUNK_SIZE_BYTES as usize) {
            chunk_num += 1;
            let chunk_name = format!("{}.part{}", original_filename, chunk_num);

            let message = self
                .send_document(chunk.to_vec(), &chunk_name, first_message_id)
                .await?;

            if first_message_id.is_none() {
                first_message_id = Some(message.message_id);
            }

            let doc = message.document.ok_or("No document in chunk response")?;
            let composite = format!("{}:{}", message.message_id, doc.file_id);
            chunk_ids.push(composite);
        }

        // Create manifest
        let mut manifest = String::from("tgstate-blob\n");
        manifest.push_str(original_filename);
        manifest.push('\n');
        for cid in &chunk_ids {
            manifest.push_str(cid);
            manifest.push('\n');
        }

        let manifest_name = format!("{}.manifest", original_filename);
        let message = self
            .send_document(manifest.into_bytes(), &manifest_name, first_message_id)
            .await?;

        let doc = message.document.ok_or("No document in manifest response")?;
        let manifest_composite = format!("{}:{}", message.message_id, doc.file_id);

        let short_id =
            database::add_file_metadata(db_path, original_filename, &manifest_composite, total_size)?;
        Ok(short_id)
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
            .timeout(std::time::Duration::from_secs(60))
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
