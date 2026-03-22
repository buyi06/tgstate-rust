use tokio::sync::broadcast;

#[derive(Clone)]
pub struct BroadcastEventBus {
    sender: broadcast::Sender<String>,
}

impl BroadcastEventBus {
    pub fn new(capacity: usize) -> Self {
        let (sender, _) = broadcast::channel(capacity);
        Self { sender }
    }

    pub fn subscribe(&self) -> broadcast::Receiver<String> {
        self.sender.subscribe()
    }

    pub fn publish(&self, data: String) {
        // Ignore error (no subscribers)
        let _ = self.sender.send(data);
    }
}

pub fn build_file_event(
    action: &str,
    file_id: &str,
    filename: Option<&str>,
    filesize: Option<i64>,
    upload_date: Option<&str>,
    short_id: Option<&str>,
) -> serde_json::Value {
    serde_json::json!({
        "action": action,
        "file_id": file_id,
        "filename": filename,
        "filesize": filesize,
        "upload_date": upload_date,
        "short_id": short_id,
    })
}
