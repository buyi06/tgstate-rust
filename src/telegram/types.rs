use serde::Deserialize;

#[derive(Debug, Deserialize)]
pub struct TelegramResponse<T> {
    pub ok: bool,
    pub result: Option<T>,
    pub description: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct Update {
    pub update_id: i64,
    pub message: Option<Message>,
    pub channel_post: Option<Message>,
    pub edited_message: Option<Message>,
    pub edited_channel_post: Option<Message>,
}

#[derive(Debug, Deserialize)]
pub struct Message {
    pub message_id: i64,
    pub chat: Chat,
    pub text: Option<String>,
    pub document: Option<Document>,
    pub photo: Option<Vec<PhotoSize>>,
    pub date: Option<i64>,
    pub reply_to_message: Option<Box<Message>>,
}

#[derive(Debug, Deserialize)]
pub struct Chat {
    pub id: i64,
    pub username: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct Document {
    pub file_id: String,
    pub file_name: Option<String>,
    pub file_size: Option<i64>,
}

#[derive(Debug, Deserialize)]
#[allow(dead_code)]
pub struct PhotoSize {
    pub file_id: String,
    pub file_size: Option<i64>,
    pub width: Option<i32>,
    pub height: Option<i32>,
}

#[derive(Debug, Deserialize)]
#[allow(dead_code)]
pub struct TelegramFile {
    pub file_id: String,
    pub file_path: Option<String>,
}

#[derive(Debug, Deserialize)]
#[allow(dead_code)]
pub struct BotUser {
    pub username: Option<String>,
}
