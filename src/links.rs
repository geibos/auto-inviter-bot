//! Жизненный цикл invite-ссылок: одна активная на запись, get-or-create (design.md D3).

use anyhow::{Context, Result};
use teloxide::payloads::CreateChatInviteLinkSetters;
use teloxide::prelude::*;
use teloxide::types::ChatId;

use crate::{db, App};

/// Имя ссылки — позывной, обрезанный до лимита Bot API (32 символа).
pub fn link_name(callsign: &str) -> String {
    callsign.chars().take(32).collect()
}

/// Get-or-create: переиспользует активную ссылку, иначе создаёт новую.
///
/// cancel-safe: NO — отмена между createChatInviteLink и save_link оставит в Telegram
/// «осиротевшую» именованную ссылку без записи в БД. Dispatcher не отменяет хендлеры
/// в полёте (кроме shutdown); худший случай — неиспользуемая ссылка, лечится /reissue.
pub async fn ensure_link(
    bot: &Bot,
    app: &App,
    chat: &db::BoundChat,
    entry: &db::Entry,
) -> Result<db::Link> {
    if let Some(link) = db::active_link(&app.pool, entry.id).await? {
        return Ok(link);
    }
    // Сериализация создания: inline-запросы летят на каждый кейстроук, без замка
    // конкурентные ensure_link создали бы несколько ссылок в Telegram (§B13).
    let _guard = app.link_lock.lock().await;
    if let Some(link) = db::active_link(&app.pool, entry.id).await? {
        return Ok(link);
    }
    let name = link_name(&entry.callsign);
    let created = bot
        .create_chat_invite_link(ChatId(chat.tg_chat_id))
        .name(name.clone())
        .member_limit(1)
        .await
        .context("createChatInviteLink не прошёл (есть ли у бота право «Invite Users»?)")?;
    match db::save_link(&app.pool, entry.id, &created.invite_link, &name).await {
        Ok(link) => Ok(link),
        Err(e) => {
            // Ссылка уже есть в Telegram, а в БД не записалась — отзываем, иначе
            // в группе копятся «осиротевшие» именные ссылки (ревью: silent-failures).
            if let Err(re) = bot
                .revoke_chat_invite_link(ChatId(chat.tg_chat_id), created.invite_link.clone())
                .await
            {
                tracing::error!(
                    name = %name, error = %re,
                    "осиротевшая ссылка: save_link и revoke оба упали — отзовите вручную (имя ссылки = позывной)"
                );
            }
            Err(e)
        }
    }
}

/// Отзывает активную ссылку записи (в Telegram и в БД). Ok(None) — активной не было.
///
/// cancel-safe: NO — отмена между revoke в Telegram и пометкой в БД оставит запись
/// с «активной» в БД, но отозванной в Telegram ссылкой; повторный /reissue это чинит.
pub async fn revoke_active(
    bot: &Bot,
    app: &App,
    chat: &db::BoundChat,
    roster_id: i64,
) -> Result<Option<()>> {
    let Some(link) = db::active_link(&app.pool, roster_id).await? else {
        return Ok(None);
    };
    bot.revoke_chat_invite_link(ChatId(chat.tg_chat_id), link.invite_link.clone())
        .await
        .context("revokeChatInviteLink не прошёл")?;
    db::mark_link_revoked(&app.pool, link.id, db::now_ts()).await?;
    Ok(Some(()))
}

/// Перевыпуск: отозвать активную (если есть) + создать новую. Работает и для вступивших.
///
/// cancel-safe: NO — композиция двух не-cancel-safe операций, см. выше.
pub async fn reissue(
    bot: &Bot,
    app: &App,
    chat: &db::BoundChat,
    entry: &db::Entry,
) -> Result<db::Link> {
    revoke_active(bot, app, chat, entry.id).await?;
    ensure_link(bot, app, chat, entry).await
}

#[cfg(test)]
mod tests {
    use super::link_name;

    #[test]
    fn link_name_truncates_to_32_chars_on_boundary() {
        let long = "Оченьдлинныйпозывнойкоторыйнепомещается";
        let name = link_name(long);
        assert_eq!(name.chars().count(), 32);
        // кириллица: обрезка по символам, не по байтам
        assert!(long.starts_with(&name));
    }

    #[test]
    fn link_name_short_unchanged() {
        assert_eq!(link_name("Сокол"), "Сокол");
    }
}
