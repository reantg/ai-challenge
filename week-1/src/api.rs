use std::time::Duration;

use reqwest::{Client, StatusCode, header::ACCEPT};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    System,
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

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ResponseFormat {
    PlainText,
    JsonObject(String),
    Markdown(String),
    Yaml(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LengthLimit {
    Default,
    MaxTokens(u64),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StopCondition {
    Natural,
    Sequence(String),
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Temperature {
    Default,
    Value(f64),
}

#[derive(Clone, Debug, PartialEq)]
pub struct CompletionOptions {
    pub response_format: ResponseFormat,
    pub length_limit: LengthLimit,
    pub stop_condition: StopCondition,
    pub temperature: Temperature,
}

impl Default for CompletionOptions {
    fn default() -> Self {
        Self {
            response_format: ResponseFormat::PlainText,
            length_limit: LengthLimit::Default,
            stop_condition: StopCondition::Natural,
            temperature: Temperature::Default,
        }
    }
}

#[derive(Deserialize)]
struct ModelsResponse {
    object: String,
    data: Vec<ModelSummary>,
}

#[derive(Deserialize)]
struct ModelSummary {
    object: String,
    #[serde(rename = "owned_by")]
    _owned_by: String,
    id: String,
}

pub async fn list_models(api_url: &str, api_key: &str) -> Result<Vec<String>, String> {
    let models_url = models_url(api_url)?;
    let client = Client::builder()
        .timeout(Duration::from_secs(120))
        .build()
        .map_err(format_http_error)?;
    let response = client
        .get(models_url)
        .header(ACCEPT, "application/json")
        .bearer_auth(api_key)
        .send()
        .await
        .map_err(format_http_error)?;
    let status = response.status();
    let body = response.text().await.map_err(format_http_error)?;
    if !status.is_success() {
        return Err(format_api_error(status, &body));
    }

    let models = decode_models_response(&body)?;
    if models.is_empty() {
        Err("DeepSeek API вернул пустой список моделей".into())
    } else {
        Ok(models)
    }
}

fn models_url(api_url: &str) -> Result<reqwest::Url, String> {
    let mut url =
        reqwest::Url::parse(api_url).map_err(|_| "Некорректный DEEPSEEK_API_URL".to_owned())?;
    let path = url.path().trim_end_matches('/');
    let prefix = path
        .strip_suffix("/chat/completions")
        .ok_or_else(|| "DEEPSEEK_API_URL должен оканчиваться на /chat/completions".to_owned())?;
    url.set_path(&format!("{prefix}/models"));
    url.set_query(None);
    url.set_fragment(None);
    Ok(url)
}

fn decode_models_response(body: &str) -> Result<Vec<String>, String> {
    let response = serde_json::from_str::<ModelsResponse>(body)
        .map_err(|_| "Не удалось разобрать список моделей DeepSeek API".to_owned())?;
    if response.object != "list" || response.data.iter().any(|model| model.object != "model") {
        return Err("DeepSeek API вернул некорректный список моделей".into());
    }
    Ok(response
        .data
        .into_iter()
        .map(|model| model.id)
        .filter(|id| !id.is_empty())
        .collect())
}

pub async fn complete(
    api_url: &str,
    api_key: &str,
    model: &str,
    messages: &[Message],
) -> Result<String, String> {
    complete_with_options(
        api_url,
        api_key,
        model,
        messages,
        &CompletionOptions::default(),
    )
    .await
}

pub async fn complete_with_options(
    api_url: &str,
    api_key: &str,
    model: &str,
    messages: &[Message],
    options: &CompletionOptions,
) -> Result<String, String> {
    let client = Client::builder()
        .timeout(Duration::from_secs(120))
        .build()
        .map_err(format_http_error)?;

    let response = client
        .post(api_url)
        .bearer_auth(api_key)
        .json(&request_value(model, messages, options, false))
        .send()
        .await
        .map_err(|error| {
            if error.is_builder() {
                "Некорректный DEEPSEEK_API_URL".to_owned()
            } else {
                format_http_error(error)
            }
        })?;

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
    options: &CompletionOptions,
    mut on_chunk: F,
) -> Result<String, String>
where
    F: FnMut(&str),
{
    let client = Client::builder()
        .timeout(Duration::from_secs(120))
        .build()
        .map_err(format_http_error)?;
    let mut response = client
        .post(api_url)
        .bearer_auth(api_key)
        .json(&request_value(model, messages, options, true))
        .send()
        .await
        .map_err(|error| {
            if error.is_builder() {
                "Некорректный DEEPSEEK_API_URL".to_owned()
            } else {
                format_http_error(error)
            }
        })?;

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
    encode_request_with_options(model, messages, &CompletionOptions::default())
}

pub fn encode_request_with_options(
    model: &str,
    messages: &[Message],
    options: &CompletionOptions,
) -> String {
    serde_json::to_string(&request_value(model, messages, options, false))
        .expect("request consists only of serializable values")
}

fn request_value(
    model: &str,
    messages: &[Message],
    options: &CompletionOptions,
    stream: bool,
) -> Value {
    let mut value = json!({
        "model": model,
        "messages": messages_with_instructions(messages, options),
        "stream": stream,
    });
    let object = value.as_object_mut().expect("JSON literal is an object");

    if matches!(options.response_format, ResponseFormat::JsonObject(_)) {
        object.insert("response_format".into(), json!({"type": "json_object"}));
    }
    if let LengthLimit::MaxTokens(value) = options.length_limit {
        object.insert("max_tokens".into(), json!(value));
    }
    if let StopCondition::Sequence(value) = &options.stop_condition {
        object.insert("stop".into(), json!(value));
    }
    if let Temperature::Value(value) = options.temperature {
        object.insert("temperature".into(), json!(value));
    }
    value
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

    let response: Response = serde_json::from_str(body)
        .map_err(|_| "Не удалось разобрать ответ DeepSeek API".to_owned())?;
    let content = response
        .choices
        .into_iter()
        .next()
        .map(|choice| choice.message.content)
        .filter(|content| !content.is_empty())
        .ok_or_else(|| "DeepSeek API вернул пустой ответ".to_owned())?;
    Ok(content)
}

fn messages_with_instructions(messages: &[Message], options: &CompletionOptions) -> Vec<Message> {
    let instructions = [
        format_instruction(&options.response_format),
        stop_instruction(&options.stop_condition),
    ]
    .into_iter()
    .flatten()
    .collect::<Vec<_>>();

    let mut result = Vec::with_capacity(messages.len() + usize::from(!instructions.is_empty()));
    if !instructions.is_empty() {
        result.push(Message::new(Role::System, instructions.join("\n")));
    }
    result.extend_from_slice(messages);
    result
}

fn format_instruction(format: &ResponseFormat) -> Option<String> {
    match format {
        ResponseFormat::PlainText => None,
        ResponseFormat::JsonObject(schema) => Some(format!(
            "Формат ответа: только один валидный JSON-объект строго по заданной \
             пользователем схеме: {schema}. Не добавляй другие поля, Markdown или текст вне JSON."
        )),
        ResponseFormat::Markdown(template) => Some(format!(
            "Формат ответа: Markdown строго по заданному пользователем шаблону: {template}"
        )),
        ResponseFormat::Yaml(schema) => Some(format!(
            "Формат ответа: только валидный YAML строго по заданной пользователем \
             схеме: {schema}. Не добавляй Markdown или текст вне YAML."
        )),
    }
}

fn stop_instruction(condition: &StopCondition) -> Option<String> {
    match condition {
        StopCondition::Natural => None,
        StopCondition::Sequence(value) => Some(format!(
            "Условие завершения: сразу после полного ответа добавь маркер `{value}` и ничего не пиши после него."
        )),
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
    }

    #[test]
    fn configured_request_contains_only_selected_parameters() {
        let options = CompletionOptions {
            response_format: ResponseFormat::JsonObject(
                r#"{"car":"...","oil":["..."],"volume":"..."}"#.into(),
            ),
            length_limit: LengthLimit::MaxTokens(512),
            stop_condition: StopCondition::Sequence("<END_OF_RESPONSE>".into()),
            temperature: Temperature::Value(0.7),
        };
        let body = encode_request_with_options(
            "deepseek-v4-flash",
            &[Message::new(Role::User, "Что такое Gleam?")],
            &options,
        );
        assert!(body.contains("\"response_format\":{\"type\":\"json_object\"}"));
        assert!(body.contains("\"max_tokens\":512"));
        assert!(body.contains("\"stop\":\"<END_OF_RESPONSE>\""));
        assert!(body.contains("\"role\":\"system\""));
        assert!(body.contains("\"temperature\":0.7"));
        assert!(!body.contains("\"top_p\""));
    }

    #[test]
    fn default_request_leaves_optional_parameters_to_api_defaults() {
        let body = encode_request("deepseek-v4-flash", &[Message::new(Role::User, "Привет")]);
        assert!(!body.contains("\"response_format\""));
        assert!(!body.contains("\"max_tokens\""));
        assert!(!body.contains("\"stop\""));
        assert!(!body.contains("\"temperature\""));
        assert!(!body.contains("\"role\":\"system\""));
    }

    #[test]
    fn markdown_format_is_only_a_system_instruction() {
        let options = CompletionOptions {
            response_format: ResponseFormat::Markdown("## Результат".into()),
            ..CompletionOptions::default()
        };
        let body = encode_request_with_options(
            "deepseek-v4-flash",
            &[Message::new(Role::User, "Привет")],
            &options,
        );
        assert!(body.contains("Markdown"));
        assert!(!body.contains("\"response_format\""));
    }

    #[test]
    fn models_url_reuses_completion_api_prefix() {
        let url = models_url("https://api.example.com/v1/chat/completions?source=test").unwrap();

        assert_eq!(url.as_str(), "https://api.example.com/v1/models");
    }

    #[test]
    fn model_list_decodes_available_identifiers() {
        let models = decode_models_response(
            r#"{"object":"list","data":[
                {"id":"deepseek-v4-flash","object":"model","owned_by":"deepseek"},
                {"id":"deepseek-v4-pro","object":"model","owned_by":"deepseek"}
            ]}"#,
        )
        .unwrap();

        assert_eq!(models, ["deepseek-v4-flash", "deepseek-v4-pro"]);
    }

    #[test]
    fn model_list_rejects_unexpected_object_types() {
        let error = decode_models_response(
            r#"{"object":"model","data":[
                {"id":"deepseek-v4-pro","object":"list","owned_by":"deepseek"}
            ]}"#,
        )
        .unwrap_err();

        assert_eq!(error, "DeepSeek API вернул некорректный список моделей");
    }

    #[test]
    fn response_content_is_decoded_without_format_validation() {
        let body = r#"{"choices":[{"message":{"role":"assistant","content":"это не JSON"}}]}"#;
        assert_eq!(decode_response(body), Ok("это не JSON".into()));
    }

    #[test]
    fn empty_choices_are_rejected() {
        assert_eq!(
            decode_response(r#"{"choices":[]}"#),
            Err("DeepSeek API вернул пустой ответ".into())
        );
    }

    #[test]
    fn empty_content_is_rejected() {
        let body = r#"{"choices":[{"message":{"content":""}}]}"#;
        assert_eq!(
            decode_response(body),
            Err("DeepSeek API вернул пустой ответ".into())
        );
    }

    #[test]
    fn malformed_response_is_rejected() {
        assert_eq!(
            decode_response("not JSON"),
            Err("Не удалось разобрать ответ DeepSeek API".into())
        );
    }

    #[test]
    fn streaming_request_enables_stream_field() {
        let value = request_value(
            "deepseek-v4-flash",
            &[Message::new(Role::User, "Задача")],
            &CompletionOptions::default(),
            true,
        );
        assert_eq!(value["stream"], true);
    }

    #[test]
    fn sse_chunks_are_decoded_and_accumulated() {
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
                b"data: {\"choices\":[{\"delta\":{\"content\":\"world\"}}]}\r\n",
                &mut answer,
                &mut collect,
            )
            .unwrap();
            process_sse_line(b"data: [DONE]\n", &mut answer, &mut collect).unwrap();
        }
        assert_eq!(answer, "Hello world");
        assert_eq!(chunks, ["Hello ", "world"]);
    }
}
