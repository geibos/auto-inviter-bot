mod auth;
mod commands;
mod db;
mod inline;
mod links;
mod membership;
mod parser;

use std::sync::Arc;

use anyhow::Context;
use teloxide::{dptree, prelude::*};

use crate::commands::Command;

/// Общее состояние, инжектится в хендлеры через dptree::deps.
pub struct App {
    pub pool: sqlx::SqlitePool,
    pub auth: auth::AuthCache,
    /// Сериализует создание invite-ссылок: защита от гонки get-or-create (design.md D3).
    pub link_lock: tokio::sync::Mutex<()>,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    dotenvy::dotenv().ok();
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info,sqlx=warn".into()),
        )
        .init();

    let token = std::env::var("BOT_TOKEN")
        .context("BOT_TOKEN не задан: укажите токен бота в переменной окружения или .env")?;
    let db_path = std::env::var("DATABASE_PATH").unwrap_or_else(|_| "bot.db".to_string());

    let pool = db::connect(&db_path).await?;
    tracing::info!(db = %db_path, "БД готова, миграции применены");

    let bot = Bot::new(token);
    let me = bot
        .get_me()
        .await
        .context("getMe не прошёл — токен невалиден или нет сети")?;
    tracing::info!("запущен как @{}", me.username());

    let app = Arc::new(App {
        pool,
        auth: auth::AuthCache::new(),
        link_lock: tokio::sync::Mutex::new(()),
    });

    // Dispatcher выводит allowed_updates автоматически из дерева хендлеров
    // (UpdateListener::hint_allowed_updates). Фиксируем ожидаемый набор в логе —
    // у автодетекта известны edge cases (design.md D8); chat_member не входит в
    // default-набор Telegram и обязан попасть сюда.
    tracing::info!(
        "ожидаемые allowed_updates: message, inline_query, chosen_inline_result, \
         callback_query, my_chat_member, chat_member"
    );

    let handler = dptree::entry()
        .branch(Update::filter_my_chat_member().endpoint(membership::on_my_chat_member))
        .branch(Update::filter_chat_member().endpoint(membership::on_chat_member))
        .branch(
            Update::filter_message()
                .filter_command::<Command>()
                .endpoint(commands::on_command),
        )
        .branch(Update::filter_message().endpoint(commands::on_message))
        .branch(Update::filter_inline_query().endpoint(inline::on_inline_query))
        .branch(Update::filter_chosen_inline_result().endpoint(inline::on_chosen))
        .branch(Update::filter_callback_query().endpoint(inline::on_callback));

    Dispatcher::builder(bot, handler)
        .dependencies(dptree::deps![app])
        .enable_ctrlc_handler()
        .build()
        .dispatch()
        .await;

    Ok(())
}
