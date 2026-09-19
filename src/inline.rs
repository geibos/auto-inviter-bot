//! Inline-выдача ссылок (spec: inline-link-delivery).
//!
//! InlineQuery не содержит chat id (только chat_type) — позывной передаётся
//! аргументом запроса, см. design.md D2. Создание записей налету — карточный
//! флоу через chosen_inline_result + editMessageText, см. design.md D11.

use std::sync::Arc;

use anyhow::Result;
use teloxide::payloads::AnswerInlineQuerySetters;
use teloxide::prelude::*;
use teloxide::types::{
    CallbackQuery, ChosenInlineResult, InlineKeyboardButton, InlineKeyboardMarkup, InlineQuery,
    InlineQueryId, InlineQueryResult, InlineQueryResultArticle, InputMessageContent,
    InputMessageContentText,
};

use crate::auth::{self, Access};
use crate::{commands, db, links, parser, App};

const MAX_RESULTS: i64 = 10;
/// Результаты зависят от прав вызывающего и состояния реестра — кэш короткий, персональный.
const CACHE_TIME_SECS: u32 = 2;
/// result_id карточки «Новая ссылка» (гостевая запись без позывного).
const GUEST_RESULT_ID: &str = "guest";
/// result_id карточки «Создать <позывной>».
const CREATE_RESULT_ID: &str = "new";
const PLACEHOLDER_TEXT: &str = "⏳ Готовлю приглашение…";

/// cancel-safe: NO — транзитивно вызывает ensure_link (links.rs): отмена между
/// createChatInviteLink и save_link оставит «осиротевшую» ссылку в Telegram.
/// Dispatcher не отменяет хендлеры в полёте (кроме shutdown).
pub async fn on_inline_query(bot: Bot, app: Arc<App>, q: InlineQuery) -> Result<()> {
    let chat = match auth::check(&bot, &app, q.from.id).await? {
        Access::Operator(chat) => chat,
        // Не-оператор и отсутствие привязки — пустой ответ, реестр не раскрывается.
        Access::NoBinding | Access::Denied => {
            return answer(&bot, q.id, Vec::new()).await;
        }
    };

    let query = q.query.trim();
    let mut results: Vec<InlineQueryResult> = Vec::new();

    if query.is_empty() {
        // Вариант по умолчанию: гостевая ссылка без позывного (spec: анонимная ссылка).
        results.push(action_card(
            GUEST_RESULT_ID,
            "➕ Новая ссылка",
            "создать гостевую запись (Гость-N) и выдать одноразовую ссылку",
        ));
        for entry in db::pending_entries(&app.pool, chat.id, MAX_RESULTS).await? {
            results.push(build_result(&bot, &app, &chat, &entry).await);
        }
    } else {
        let found = db::find_entries(&app.pool, chat.id, query, MAX_RESULTS).await?;
        for entry in &found {
            results.push(build_result(&bot, &app, &chat, entry).await);
        }
        // Карточка «Создать» — только при отсутствии точного совпадения
        // (spec: создание записи налету через карточку).
        if let Some(p) = valid_quick(query) {
            let exact = db::entry_by_callsign(&app.pool, chat.id, &p.callsign)
                .await?
                .is_some();
            if !exact {
                let username = p
                    .username
                    .as_deref()
                    .map(|u| format!(" (@{u})"))
                    .unwrap_or_default();
                results.push(action_card(
                    CREATE_RESULT_ID,
                    &format!("➕ Создать «{}»{username}", p.callsign),
                    "создать запись и выдать одноразовую ссылку",
                ));
            }
        } else if found.is_empty() {
            results.push(hint_card(
                "Ничего не найдено",
                "Для создания налету наберите: позывной [@username]",
            ));
        }
    }

    answer(&bot, q.id, results).await
}

/// Создание по выбранной карточке: событие приходит после отправки заглушки —
/// только если у BotFather включён inline feedback (design.md D11).
///
/// cancel-safe: NO — между созданием записи/ссылки и editMessageText отмена
/// оставит заглушку неотредактированной; запись и ссылка при этом корректны,
/// повторный выбор карточки выдаст ту же ссылку.
pub async fn on_chosen(bot: Bot, app: Arc<App>, chosen: ChosenInlineResult) -> Result<()> {
    // inline_message_id есть только у карточек с клавиатурой — наших карточек-действий.
    let Some(inline_message_id) = chosen.inline_message_id.clone() else {
        return Ok(());
    };
    let is_guest = chosen.result_id == GUEST_RESULT_ID;
    if !is_guest && chosen.result_id != CREATE_RESULT_ID {
        return Ok(());
    }
    // Карточки видят только операторы, но событие проверяем независимо (defense in depth).
    let chat = match auth::check(&bot, &app, chosen.from.id).await? {
        Access::Operator(chat) => chat,
        Access::NoBinding | Access::Denied => return Ok(()),
    };
    let created_by = chosen.from.id.0 as i64;

    let entry = if is_guest {
        // Каждый выбор гостевой карточки — осознанно новый гость (spec).
        db::create_guest_entry(&app.pool, chat.id, created_by).await?
    } else {
        let Some(p) = valid_quick(chosen.query.trim()) else {
            bot.edit_message_text_inline(inline_message_id, "Не удалось разобрать запрос.")
                .await?;
            return Ok(());
        };
        // Повторный выбор → Duplicate → reuse (spec: повторный выбор той же карточки).
        match db::add_entry(&app.pool, chat.id, &p.callsign, p.username.as_deref(), created_by)
            .await?
        {
            db::AddOutcome::Added(e) | db::AddOutcome::Duplicate(e) => e,
        }
    };

    let text = if entry.joined_at.is_some() {
        let who = commands::display_person(
            entry.joined_name.as_deref(),
            entry.joined_username.as_deref(),
            &entry.callsign,
        );
        format!("{who} уже в группе.")
    } else {
        match links::ensure_link(&bot, &app, &chat, &entry).await {
            Ok(link) => format!(
                "Личное приглашение в группу:\n{}\nСсылка персональная и одноразовая.",
                link.invite_link
            ),
            Err(e) => {
                tracing::warn!(callsign = %entry.callsign, error = %e, "ensure_link по chosen не прошёл");
                "Не удалось создать приглашение, попробуйте позже.".to_string()
            }
        }
    };
    bot.edit_message_text_inline(inline_message_id, text).await?;
    Ok(())
}

/// Гасит спиннер декоративной кнопки заглушки.
///
/// cancel-safe: yes — единственный идемпотентный вызов.
pub async fn on_callback(bot: Bot, q: CallbackQuery) -> Result<()> {
    bot.answer_callback_query(q.id).await?;
    Ok(())
}

async fn answer(bot: &Bot, query_id: InlineQueryId, results: Vec<InlineQueryResult>) -> Result<()> {
    bot.answer_inline_query(query_id, results)
        .is_personal(true)
        .cache_time(CACHE_TIME_SECS)
        .await?;
    Ok(())
}

/// Валидный одиночный `позывной [@username]` для quick-add; иначе None.
fn valid_quick(q: &str) -> Option<parser::ParsedEntry> {
    match parser::parse_roster_lines(q).pop() {
        Some(Ok(p)) => Some(p),
        _ => None,
    }
}

/// Карточка-действие: отправляет заглушку с декоративной клавиатурой — без
/// клавиатуры ChosenInlineResult не несёт inline_message_id и сообщение
/// было бы нечем редактировать.
fn action_card(id: &str, title: &str, description: &str) -> InlineQueryResult {
    let keyboard = InlineKeyboardMarkup::new([[InlineKeyboardButton::callback("⏳", "noop")]]);
    InlineQueryResultArticle::new(
        id.to_string(),
        title.to_string(),
        InputMessageContent::Text(InputMessageContentText::new(PLACEHOLDER_TEXT)),
    )
    .description(description.to_string())
    .reply_markup(keyboard)
    .into()
}

/// Карточка-подсказка. При выборе отправляет текст подсказки (безвредно).
fn hint_card(title: &str, description: &str) -> InlineQueryResult {
    InlineQueryResultArticle::new(
        "hint".to_string(),
        title.to_string(),
        InputMessageContent::Text(InputMessageContentText::new(description.to_string())),
    )
    .description(description.to_string())
    .into()
}

/// Карточка существующей записи. Ошибка создания ссылки не роняет весь ответ —
/// оператор видит карточку с текстом ошибки.
async fn build_result(
    bot: &Bot,
    app: &App,
    chat: &db::BoundChat,
    entry: &db::Entry,
) -> InlineQueryResult {
    let username = entry
        .username
        .as_deref()
        .map(|u| format!("@{u}"))
        .unwrap_or_else(|| "без username".to_string());

    let (description, message) = if entry.joined_at.is_some() {
        let who = commands::display_person(
            entry.joined_name.as_deref(),
            entry.joined_username.as_deref(),
            &entry.callsign,
        );
        (
            format!("{who} · уже в группе ✅"),
            // Без ссылки: повторная выдача только явным /reissue (spec: inline-link-delivery).
            format!("{who} уже в группе."),
        )
    } else {
        match links::ensure_link(bot, app, chat, entry).await {
            Ok(link) => (
                format!("{username} · выдать ссылку"),
                // Нейтральный текст без внутренних идентификаторов (design.md D12).
                format!(
                    "Личное приглашение в группу:\n{}\nСсылка персональная и одноразовая.",
                    link.invite_link
                ),
            ),
            Err(e) => {
                tracing::warn!(callsign = %entry.callsign, error = %e, "ensure_link в inline не прошёл");
                (
                    format!("{username} · ошибка создания ссылки"),
                    "Не удалось создать приглашение, попробуйте позже.".to_string(),
                )
            }
        }
    };

    InlineQueryResultArticle::new(
        entry.id.to_string(),
        entry.callsign.clone(),
        InputMessageContent::Text(InputMessageContentText::new(message)),
    )
    .description(description)
    .into()
}

#[cfg(test)]
mod tests {
    use super::valid_quick;
    use crate::parser::ParsedEntry;

    fn entry(callsign: &str, username: Option<&str>) -> ParsedEntry {
        ParsedEntry {
            callsign: callsign.to_string(),
            username: username.map(String::from),
        }
    }

    #[test]
    fn valid_single_callsign() {
        assert_eq!(valid_quick("Новичок"), Some(entry("Новичок", None)));
    }

    #[test]
    fn valid_with_username_normalized() {
        assert_eq!(
            valid_quick("Новичок @Ivan_P"),
            Some(entry("Новичок", Some("ivan_p")))
        );
    }

    #[test]
    fn username_only_is_invalid() {
        assert_eq!(valid_quick("@ivan"), None);
    }

    #[test]
    fn extra_tokens_is_invalid() {
        assert_eq!(valid_quick("Новичок Иван @ivan"), None);
    }

    #[test]
    fn empty_is_invalid() {
        assert_eq!(valid_quick(""), None);
        assert_eq!(valid_quick("   "), None);
    }
}
