use deepseek_chat::ui;

const DEFAULT_API_URL: &str = "https://api.deepseek.com/chat/completions";
const DEFAULT_MODEL: &str = "deepseek-v4-flash";

#[tokio::main]
async fn main() {
    let api_key = match std::env::var("DEEPSEEK_API_KEY") {
        Ok(value) => value,
        Err(_) => {
            eprintln!(
                "Не задан DEEPSEEK_API_KEY. Сначала выполните:\n\
                 export DEEPSEEK_API_KEY=ваш_ключ"
            );
            return;
        }
    };

    let api_url = std::env::var("DEEPSEEK_API_URL").unwrap_or_else(|_| DEFAULT_API_URL.to_owned());
    let model = std::env::var("DEEPSEEK_MODEL").unwrap_or_else(|_| DEFAULT_MODEL.to_owned());

    if let Err(error) = ui::run(api_url, api_key, model).await {
        eprintln!("Ошибка терминального интерфейса: {error}");
    }
}
