use dotenvy::dotenv;
use log::{error, info, warn};
use reqwest::Client as HttpClient;
use serde::Deserialize;
use std::env;
use std::fs;
use std::path::Path;
use std::time::Duration;
use telers::{
    methods::{SendMessage, SendPhoto},
    types::InputFile,
    Bot,
};

const STATE_FILE: &str = "last_post_id.txt";

#[derive(Debug, Deserialize)]
struct VkApiResponse<T> {
    response: Option<T>,
    error: Option<VkError>,
}

#[derive(Debug, Deserialize)]
struct VkError {
    error_code: i64,
    error_msg: String,
}

#[derive(Debug, Deserialize)]
struct WallGetResponse {
    items: Vec<VkPost>,
}

#[derive(Debug, Deserialize, Clone)]
struct VkPost {
    id: i64,
    text: String,
    #[serde(default)]
    is_pinned: Option<i32>,
    #[serde(default)]
    attachments: Vec<VkAttachment>,
}

#[derive(Debug, Deserialize, Clone)]
struct VkAttachment {
    #[serde(rename = "type")]
    attach_type: String,
    photo: Option<VkPhoto>,
}

#[derive(Debug, Deserialize, Clone)]
struct VkPhoto {
    sizes: Vec<VkPhotoSize>,
}

#[derive(Debug, Deserialize, Clone)]
struct VkPhotoSize {
    url: String,
    width: u32,
    height: u32,
}

fn get_best_photo_url(photo: &VkPhoto) -> Option<String> {
    photo
        .sizes
        .iter()
        .max_by_key(|s| (s.width * s.height).max(1))
        .map(|s| s.url.clone())
        .or_else(|| photo.sizes.last().map(|s| s.url.clone()))
}

fn read_last_id() -> Option<i64> {
    if Path::new(STATE_FILE).exists() {
        fs::read_to_string(STATE_FILE)
            .ok()
            .and_then(|s| s.trim().parse::<i64>().ok())
    } else {
        None
    }
}

fn save_last_id(id: i64) -> Result<(), std::io::Error> {
    fs::write(STATE_FILE, id.to_string())
}

async fn fetch_vk_posts(
    http_client: &HttpClient,
    vk_token: &str,
    group_id: &str,
) -> Result<Vec<VkPost>, Box<dyn std::error::Error>> {
    let owner_id = format!("-{}", group_id);
    let url = "https://api.vk.com/method/wall.get";

    let resp = http_client
        .get(url)
        .query(&[
            ("owner_id", owner_id.as_str()),
            ("count", "10"),
            ("filter", "owner"),
            ("access_token", vk_token),
            ("v", "5.199"),
        ])
        .send()
        .await?
        .json::<VkApiResponse<WallGetResponse>>()
        .await?;

    if let Some(err) = resp.error {
        return Err(format!("VK API Error [{}]: {}", err.error_code, err.error_msg).into());
    }

    Ok(resp.response.map(|r| r.items).unwrap_or_default())
}

async fn send_post_to_telegram(
    bot: &Bot,
    bot_token: &str,
    http_client: &HttpClient,
    chat_id: &str,
    post: &VkPost,
) -> Result<(), Box<dyn std::error::Error>> {
    // Собираем ВСЕ фотографии к посту
    let photo_urls: Vec<String> = post
        .attachments
        .iter()
        .filter(|att| att.attach_type == "photo")
        .filter_map(|att| att.photo.as_ref().and_then(get_best_photo_url))
        .collect();

    let text = post.text.trim();

    if photo_urls.len() > 1 {
        // Больше 1 фото -> собираем альбом (до 10 фото)
        let count = photo_urls.len().min(10);
        let mut media_list = Vec::new();
        let has_short_caption = !text.is_empty() && text.chars().count() <= 1024;

        for (i, url) in photo_urls.into_iter().take(count).enumerate() {
            let mut obj = serde_json::json!({
                "type": "photo",
                "media": url,
            });
            if i == 0 && has_short_caption {
                obj["caption"] = serde_json::Value::String(text.to_string());
            }
            media_list.push(obj);
        }

        let tg_url = format!("https://api.telegram.org/bot{}/sendMediaGroup", bot_token);
        let resp = http_client
            .post(&tg_url)
            .json(&serde_json::json!({
                "chat_id": chat_id,
                "media": media_list,
            }))
            .send()
            .await?;

        if !resp.status().is_success() {
            let err_text = resp.text().await.unwrap_or_default();
            return Err(format!("TG sendMediaGroup error: {}", err_text).into());
        }

        if !text.is_empty() && text.chars().count() > 1024 {
            bot.send(SendMessage::new(chat_id, text)).await?;
        }
    } else if photo_urls.len() == 1 {
        let url = &photo_urls[0];
        if text.chars().count() <= 1024 {
            let mut req = SendPhoto::new(chat_id, InputFile::url(url));
            if !text.is_empty() {
                req = req.caption(text);
            }
            bot.send(req).await?;
        } else {
            bot.send(SendPhoto::new(chat_id, InputFile::url(url))).await?;
            bot.send(SendMessage::new(chat_id, text)).await?;
        }
    } else if !text.is_empty() {
        bot.send(SendMessage::new(chat_id, text)).await?;
    }

    Ok(())
}

#[tokio::main]
async fn main() {
    dotenv().ok();
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    info!("Запуск бота репоста VK -> Telegram...");

    let bot_token = env::var("BOT_TOKEN").expect("BOT_TOKEN не установлен в .env");
    let vk_token = env::var("VK_ACCESS_TOKEN").expect("VK_ACCESS_TOKEN не установлен в .env");
    let vk_group_id = env::var("VK_GROUP_ID").expect("VK_GROUP_ID не установлен в .env");
    let tg_channel_id = env::var("TG_CHANNEL_ID").expect("TG_CHANNEL_ID не установлен в .env");
    let interval_secs: u64 = env::var("CHECK_INTERVAL_SECS")
        .unwrap_or_else(|_| "1800".to_string())
        .parse()
        .unwrap_or(1800);

    let bot = Bot::from_env();
    let http_client = HttpClient::new();

    let mut last_id = read_last_id();

    if last_id.is_none() {
        info!("Первый запуск: сохраняем текущий ID последнего поста из ВК...");
        match fetch_vk_posts(&http_client, &vk_token, &vk_group_id).await {
            Ok(posts) => {
                if let Some(newest) = posts.iter().map(|p| p.id).max() {
                    let _ = save_last_id(newest);
                    last_id = Some(newest);
                    info!("Запомнен стартовый пост ID: {}", newest);
                }
            }
            Err(e) => error!("Ошибка при первом запросе к VK: {:?}", e),
        }
    }

    info!("Бот запущен. Интервал проверки: {} сек.", interval_secs);

    loop {
        if let Some(current_last_id) = last_id {
            match fetch_vk_posts(&http_client, &vk_token, &vk_group_id).await {
                Ok(posts) => {
                    let mut new_posts: Vec<VkPost> = posts
                        .into_iter()
                        .filter(|p| p.id > current_last_id)
                        .collect();

                    new_posts.sort_by_key(|p| p.id);

                    for post in new_posts {
                        info!("Публикация нового поста VK ID: {}", post.id);
                        if let Err(e) = send_post_to_telegram(
                            &bot,
                            &bot_token,
                            &http_client,
                            &tg_channel_id,
                            &post,
                        )
                        .await
                        {
                            error!("Не удалось отправить пост {} в Telegram: {:?}", post.id, e);
                            break;
                        }

                        current_last_id_updated(&mut last_id, post.id);
                        tokio::time::sleep(Duration::from_secs(2)).await;
                    }
                }
                Err(e) => {
                    warn!("Ошибка проверки стены VK: {:?}", e);
                }
            }
        }

        tokio::time::sleep(Duration::from_secs(interval_secs)).await;
    }
}

fn current_last_id_updated(last_id: &mut Option<i64>, new_id: i64) {
    *last_id = Some(new_id);
    if let Err(e) = save_last_id(new_id) {
        error!("Не удалось сохранить ID в файл: {:?}", e);
    }
}
