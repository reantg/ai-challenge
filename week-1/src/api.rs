use std::time::Duration;

use reqwest::{Client, StatusCode};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    User,
    Assistant,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Message {
    pub role: Role,
    pub content: String,
}

impl Message {
    pub fn new(role: Role, content: impl Into<String>) -> Self {
        Self {
            role,
            content: content.into(),
        }
    }
}

pub async fn complete(
    api_url: &str,
    api_key: &str,
    model: &str,
    messages: &[Message],
) -> Result<String, String> {
    let client = client()?;
    let response = client
        .post(api_url)
        .bearer_auth(api_key)
        .json(&request_value(model, messages, false))
        .send()
        .await
        .map_err(map_send_error)?;
    let status = response.status();
    let body = response.text().await.map_err(format_http_error)?;
    if status.is_success() {
        decode_response(&body)
    } else {
        Err(format_api_error(status, &body))
    }
}

pub async fn complete_streaming<F>(
    api_url: &str,
    api_key: &str,
    model: &str,
    messages: &[Message],
    mut on_chunk: F,
) -> Result<String, String>
where
    F: FnMut(&str),
{
    let mut response = client()?
        .post(api_url)
        .bearer_auth(api_key)
        .json(&request_value(model, messages, true))
        .send()
        .await
        .map_err(map_send_error)?;
    let status = response.status();
    if !status.is_success() {
        let body = response.text().await.map_err(format_http_error)?;
        return Err(format_api_error(status, &body));
    }

    let mut pending = Vec::new();
    let mut answer = String::new();
    while let Some(chunk) = response.chunk().await.map_err(format_http_error)? {
        pending.extend_from_slice(&chunk);
        while let Some(newline) = pending.iter().position(|byte| *byte == b'\n') {
            let line = pending.drain(..=newline).collect::<Vec<_>>();
            process_sse_line(&line, &mut answer, &mut on_chunk)?;
        }
    }
    if !pending.is_empty() {
        process_sse_line(&pending, &mut answer, &mut on_chunk)?;
    }
    if answer.is_empty() {
        Err("DeepSeek API вернул пустой ответ".into())
    } else {
        Ok(answer)
    }
}

pub fn encode_request(model: &str, messages: &[Message]) -> String {
    serde_json::to_string(&request_value(model, messages, false))
        .expect("request consists only of serializable values")
}

fn client() -> Result<Client, String> {
    Client::builder()
        .timeout(Duration::from_secs(120))
        .build()
        .map_err(format_http_error)
}

fn request_value(model: &str, messages: &[Message], stream: bool) -> Value {
    json!({ "model": model, "messages": messages, "stream": stream })
}

fn process_sse_line<F>(bytes: &[u8], answer: &mut String, on_chunk: &mut F) -> Result<(), String>
where
    F: FnMut(&str),
{
    let line = std::str::from_utf8(bytes)
        .map_err(|_| "DeepSeek API вернул некорректный текст".to_owned())?
        .trim_end_matches(['\r', '\n']);
    let Some(data) = line.strip_prefix("data:") else {
        return Ok(());
    };
    let data = data.trim_start();
    if data == "[DONE]" || data.is_empty() {
        return Ok(());
    }
    let value: Value = serde_json::from_str(data)
        .map_err(|_| "Не удалось разобрать поток DeepSeek API".to_owned())?;
    if let Some(message) = value.pointer("/error/message").and_then(Value::as_str) {
        return Err(format!("DeepSeek API вернул ошибку: {message}"));
    }
    if let Some(content) = value
        .pointer("/choices/0/delta/content")
        .and_then(Value::as_str)
        .filter(|content| !content.is_empty())
    {
        answer.push_str(content);
        on_chunk(content);
    }
    Ok(())
}

pub fn decode_response(body: &str) -> Result<String, String> {
    #[derive(Deserialize)]
    struct Response {
        choices: Vec<Choice>,
    }
    #[derive(Deserialize)]
    struct Choice {
        message: ResponseMessage,
    }
    #[derive(Deserialize)]
    struct ResponseMessage {
        content: String,
    }

    serde_json::from_str::<Response>(body)
        .map_err(|_| "Не удалось разобрать ответ DeepSeek API".to_owned())?
        .choices
        .into_iter()
        .next()
        .map(|choice| choice.message.content)
        .filter(|content| !content.is_empty())
        .ok_or_else(|| "DeepSeek API вернул пустой ответ".to_owned())
}

fn map_send_error(error: reqwest::Error) -> String {
    if error.is_builder() {
        "Некорректный DEEPSEEK_API_URL".to_owned()
    } else {
        format_http_error(error)
    }
}

fn format_http_error(error: reqwest::Error) -> String {
    if error.is_timeout() {
        "DeepSeek API не ответил за 120 секунд".to_owned()
    } else if error.is_connect() {
        "Не удалось подключиться к DeepSeek API".to_owned()
    } else if error.is_decode() {
        "DeepSeek API вернул некорректный текст".to_owned()
    } else {
        format!("Ошибка запроса к DeepSeek API: {error}")
    }
}

fn format_api_error(status: StatusCode, body: &str) -> String {
    let details = serde_json::from_str::<Value>(body)
        .ok()
        .and_then(|value| value.pointer("/error/message")?.as_str().map(str::to_owned))
        .map(|message| format!(": {message}"))
        .unwrap_or_default();
    format!("DeepSeek API вернул HTTP {}{details}", status.as_u16())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_contains_complete_history() {
        let body = encode_request(
            "deepseek-v4-flash",
            &[
                Message::new(Role::User, "Привет"),
                Message::new(Role::Assistant, "Здравствуйте!"),
                Message::new(Role::User, "Как дела?"),
            ],
        );
        assert!(body.contains("\"model\":\"deepseek-v4-flash\""));
        assert!(body.contains("\"role\":\"assistant\""));
        assert!(body.contains("Как дела?"));
        assert!(!body.contains("max_tokens"));
    }

    #[test]
    fn response_errors_are_reported() {
        assert_eq!(
            decode_response(r#"{"choices":[]}"#),
            Err("DeepSeek API вернул пустой ответ".into())
        );
        assert_eq!(
            decode_response("not JSON"),
            Err("Не удалось разобрать ответ DeepSeek API".into())
        );
    }

    #[test]
    fn streaming_request_and_chunks_are_supported() {
        assert_eq!(
            request_value("model", &[Message::new(Role::User, "Задача")], true)["stream"],
            true
        );
        let mut answer = String::new();
        let mut chunks = Vec::new();
        {
            let mut collect = |chunk: &str| chunks.push(chunk.to_owned());
            process_sse_line(
                b"data: {\"choices\":[{\"delta\":{\"content\":\"Hello \"}}]}\n",
                &mut answer,
                &mut collect,
            )
            .unwrap();
            process_sse_line(
                b"data: {\"choices\":[{\"delta\":{\"content\":\"world\"}}]}\n",
                &mut answer,
                &mut collect,
            )
            .unwrap();
        }
        assert_eq!(answer, "Hello world");
        assert_eq!(chunks, ["Hello ", "world"]);
    }
}
