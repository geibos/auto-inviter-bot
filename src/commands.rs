//! Команды в личке бота и bulk-импорт (specs: roster-management, operator-access-control).

use std::sync::Arc;

use anyhow::Result;
use chrono::DateTime;
use teloxide::prelude::*;
use teloxide::types::{ChatId, ChatMemberKind, Message};
use teloxide::utils::command::BotCommands;

use crate::auth::{self, Access};
use crate::{db, links, parser, App};

/// Лимит текста сообщения Telegram — 4096; режем с запасом.
const CHUNK_LIMIT: usize = 3500;
const NO_BINDING_TEXT: &str = "Бот не привязан к группе. Добавьте его в целевую группу \
                               и назначьте администратором с правом «Invite Users» (Пригласительные ссылки).";
const DENIED_TEXT: &str = "Нет доступа: команды доступны только администраторам привязанной группы.";

#[derive(BotCommands, Clone)]
#[command(rename_rule = "lowercase")]
pub enum Command {
    Start,
    Status,
    List,
    Add(String),
    Del(String),
    Reissue(String),
}

/// cancel-safe: yes на уровне диспетчеризации — каждая ветка либо read-only,
/// либо делегирует функциям с собственной аннотацией.
pub async fn on_command(bot: Bot, app: Arc<App>, msg: Message, cmd: Command) -> Result<()> {
    if !msg.chat.is_private() {
        return Ok(());
    }
    let Some(user) = msg.from.clone() else {
        return Ok(());
    };
    let access = auth::check(&bot, &app, user.id).await?;
    let chat = match access {
        Access::NoBinding => {
            bot.send_message(msg.chat.id, NO_BINDING_TEXT).await?;
            return Ok(());
        }
        Access::Denied => {
            bot.send_message(msg.chat.id, DENIED_TEXT).await?;
            return Ok(());
        }
        Access::Operator(chat) => chat,
    };

    match cmd {
        Command::Start => cmd_start(&bot, &msg).await,
        Command::Status => cmd_status(&bot, &app, &msg, &chat).await,
        Command::List => cmd_list(&bot, &app, &msg, &chat).await,
        Command::Add(args) => cmd_add(&bot, &app, &msg, &chat, &args, user.id.0 as i64).await,
        Command::Del(args) => cmd_del(&bot, &app, &msg, &chat, &args).await,
        Command::Reissue(args) => cmd_reissue(&bot, &app, &msg, &chat, &args).await,
    }
}

/// Некомандный текст в личке от оператора = bulk-импорт (design.md D7).
///
/// cancel-safe: NO — отмена посреди импорта оставит часть записей добавленной без сводки;
/// dispatcher не отменяет хендлеры в полёте.
pub async fn on_message(bot: Bot, app: Arc<App>, msg: Message) -> Result<()> {
    if !msg.chat.is_private() {
        return Ok(());
    }
    let Some(text) = msg.text() else {
        return Ok(());
    };
    if text.starts_with('/') {
        // неизвестная команда — не считаем её списком позывных
        bot.send_message(msg.chat.id, "Неизвестная команда. /start — справка.").await?;
        return Ok(());
    }
    let Some(user) = msg.from.clone() else {
        return Ok(());
    };
    match auth::check(&bot, &app, user.id).await? {
        Access::NoBinding => {
            bot.send_message(msg.chat.id, NO_BINDING_TEXT).await?;
            return Ok(());
        }
        // Не-операторам некомандный текст игнорируется без разбора (spec: operator-access-control).
        Access::Denied => return Ok(()),
        Access::Operator(chat) => {
            let report = import_lines(&bot, &app, &chat, text, user.id.0 as i64).await?;
            send_chunked(&bot, msg.chat.id, &report).await?;
        }
    }
    Ok(())
}

async fn cmd_start(bot: &Bot, msg: &Message) -> Result<()> {
    let text = "Бот персональных приглашений.\n\n\
        Команды:\n\
        /add <позывной> [@username] — добавить запись\n\
        /del <позывной> — удалить запись (ссылка отзывается)\n\
        /reissue <позывной> — перевыпустить ссылку\n\
        /list — реестр со статусами\n\
        /status — привязка, права, счётчики\n\n\
        Bulk-загрузка: пришлите список строк вида «позывной [@username]» — \
        по записи на строку, разделители: пробел, табуляция, «;», «,».\n\n\
        Выдача ссылки: в чате с кандидатом наберите @<имя_бота> <позывной> \
        и выберите карточку — в чат уйдёт персональная одноразовая ссылка.\n\n\
        Создание налету: если записи нет — выберите карточку «Создать …», \
        ссылка появится в отправленном сообщении через секунду. \
        Пустой запрос — первая карточка «Новая ссылка»: гостевая запись \
        (Гость-N) без позывного.";
    bot.send_message(msg.chat.id, text).await?;
    Ok(())
}

async fn cmd_status(bot: &Bot, app: &App, msg: &Message, chat: &db::BoundChat) -> Result<()> {
    let me = bot.get_me().await?;
    let rights = match bot.get_chat_member(ChatId(chat.tg_chat_id), me.id).await {
        Ok(m) => match m.kind {
            ChatMemberKind::Administrator(a) if a.can_invite_users => "админ + Invite Users ✓",
            ChatMemberKind::Administrator(_) => "админ, но БЕЗ права Invite Users ✗",
            _ => "НЕ админ ✗",
        }
        .to_string(),
        Err(e) => format!("не удалось проверить ({e})"),
    };
    let c = db::counts(&app.pool, chat.id).await?;
    let text = format!(
        "Группа: {} (id {})\nПривязка: {}\nПрава бота: {}\n\
         Записей: {} (вступили {}, выдано {}, ожидают {})",
        chat.title,
        chat.tg_chat_id,
        if chat.is_active { "активна" } else { "НЕАКТИВНА" },
        rights,
        c.total,
        c.joined,
        c.issued,
        c.pending(),
    );
    bot.send_message(msg.chat.id, text).await?;
    Ok(())
}

async fn cmd_list(bot: &Bot, app: &App, msg: &Message, chat: &db::BoundChat) -> Result<()> {
    let entries = db::list_entries(&app.pool, chat.id).await?;
    if entries.is_empty() {
        bot.send_message(msg.chat.id, "Реестр пуст. /add или пришлите список.").await?;
        return Ok(());
    }
    let lines: Vec<String> = entries.iter().map(format_listed).collect();
    send_chunked(bot, msg.chat.id, &lines.join("\n")).await?;
    Ok(())
}

fn format_listed(e: &db::ListedEntry) -> String {
    if let Some(joined_at) = e.joined_at {
        let when = format_ts(joined_at);
        let who = display_person(
            e.joined_name.as_deref(),
            e.joined_username.as_deref(),
            "(неизвестно)",
        );
        let mismatch = match (&e.username, &e.joined_username) {
            (Some(expected), Some(actual)) if expected != actual => {
                format!(" ⚠ ожидался @{expected}")
            }
            (Some(expected), None) => format!(" ⚠ ожидался @{expected}"),
            _ => String::new(),
        };
        return format!("✅ {} — {who}, вступил {when}{mismatch}", e.callsign);
    }
    let username = e
        .username
        .as_deref()
        .map(|u| format!("@{u}"))
        .unwrap_or_else(|| "(без username)".to_string());
    if e.has_active_link {
        // URL осознанно не показываем: выдача — через inline или /reissue (ревью: need-to-know)
        format!("🔗 {} — {username}, ссылка выдана", e.callsign)
    } else {
        format!("⏳ {} — {username}", e.callsign)
    }
}

/// «Имя Фамилия (@username)» вступившего; деградирует до доступных данных.
pub(crate) fn display_person(name: Option<&str>, username: Option<&str>, fallback: &str) -> String {
    match (name, username) {
        (Some(n), Some(u)) => format!("{n} (@{u})"),
        (Some(n), None) => n.to_string(),
        (None, Some(u)) => format!("@{u}"),
        (None, None) => fallback.to_string(),
    }
}

fn format_ts(ts: i64) -> String {
    DateTime::from_timestamp(ts, 0)
        .map(|dt| dt.format("%Y-%m-%d %H:%M UTC").to_string())
        .unwrap_or_else(|| ts.to_string())
}

async fn cmd_add(
    bot: &Bot,
    app: &App,
    msg: &Message,
    chat: &db::BoundChat,
    args: &str,
    created_by: i64,
) -> Result<()> {
    if args.trim().is_empty() {
        bot.send_message(msg.chat.id, "Формат: /add <позывной> [@username]").await?;
        return Ok(());
    }
    let report = import_lines(bot, app, chat, args, created_by).await?;
    send_chunked(bot, msg.chat.id, &report).await?;
    Ok(())
}

/// Общий путь для /add и bulk-импорта: парсинг → вставка → eager-ссылки → сводка.
async fn import_lines(
    bot: &Bot,
    app: &App,
    chat: &db::BoundChat,
    text: &str,
    created_by: i64,
) -> Result<String> {
    let parsed = parser::parse_roster_lines(text);
    if parsed.is_empty() {
        return Ok("Пустой список: ни одной непустой строки.".to_string());
    }
    let mut added: Vec<String> = Vec::new();
    let mut duplicates: Vec<String> = Vec::new();
    let mut errors: Vec<String> = Vec::new();
    let mut link_failures = 0usize;

    for item in parsed {
        match item {
            Err(e) => errors.push(format!("строка {}: {}", e.line_no, e.reason)),
            Ok(p) => {
                match db::add_entry(&app.pool, chat.id, &p.callsign, p.username.as_deref(), created_by)
                    .await?
                {
                    db::AddOutcome::Duplicate(existing) => {
                        duplicates.push(existing.callsign.clone());
                    }
                    db::AddOutcome::Added(entry) => {
                        // Eager-создание ссылки (spec: invite-link-issuance). Ошибка не
                        // блокирует импорт — ссылка будет создана лениво при inline-запросе.
                        if chat.is_active {
                            if let Err(e) = links::ensure_link(bot, app, chat, &entry).await {
                                tracing::warn!(callsign = %entry.callsign, error = %e, "eager-ссылка не создана");
                                link_failures += 1;
                            }
                        }
                        added.push(entry.callsign.clone());
                    }
                }
            }
        }
    }

    let mut report = format!("Добавлено: {}", added.len());
    if !duplicates.is_empty() {
        report.push_str(&format!("\nДубликаты ({}): {}", duplicates.len(), duplicates.join(", ")));
    }
    if !errors.is_empty() {
        report.push_str(&format!("\nОшибки ({}):\n{}", errors.len(), errors.join("\n")));
    }
    if link_failures > 0 {
        report.push_str(&format!(
            "\n⚠ Не создано ссылок: {link_failures} (проверьте права бота, /status); \
             будут созданы при первом inline-запросе."
        ));
    }
    Ok(report)
}

async fn cmd_del(bot: &Bot, app: &App, msg: &Message, chat: &db::BoundChat, args: &str) -> Result<()> {
    let callsign = args.trim();
    if callsign.is_empty() {
        bot.send_message(msg.chat.id, "Формат: /del <позывной>").await?;
        return Ok(());
    }
    let Some(entry) = db::entry_by_callsign(&app.pool, chat.id, callsign).await? else {
        bot.send_message(msg.chat.id, format!("Запись «{callsign}» не найдена.")).await?;
        return Ok(());
    };
    // Сначала отзыв ссылки, потом удаление: если отзыв упал — запись остаётся, можно повторить.
    let revoked = links::revoke_active(bot, app, chat, entry.id).await?;
    db::delete_entry(&app.pool, entry.id).await?;
    let note = if revoked.is_some() { " Активная ссылка отозвана." } else { "" };
    bot.send_message(msg.chat.id, format!("Запись «{}» удалена.{note}", entry.callsign)).await?;
    Ok(())
}

async fn cmd_reissue(
    bot: &Bot,
    app: &App,
    msg: &Message,
    chat: &db::BoundChat,
    args: &str,
) -> Result<()> {
    let callsign = args.trim();
    if callsign.is_empty() {
        bot.send_message(msg.chat.id, "Формат: /reissue <позывной>").await?;
        return Ok(());
    }
    let Some(entry) = db::entry_by_callsign(&app.pool, chat.id, callsign).await? else {
        bot.send_message(msg.chat.id, format!("Запись «{callsign}» не найдена.")).await?;
        return Ok(());
    };
    let link = links::reissue(bot, app, chat, &entry).await?;
    bot.send_message(
        msg.chat.id,
        format!("Новая ссылка для «{}»: {}", entry.callsign, link.invite_link),
    )
    .await?;
    Ok(())
}

/// Разбивает длинный текст на сообщения ≤ CHUNK_LIMIT по границам строк.
async fn send_chunked(bot: &Bot, chat_id: ChatId, text: &str) -> Result<()> {
    let mut buf = String::new();
    for line in text.lines() {
        if !buf.is_empty() && buf.len() + line.len() + 1 > CHUNK_LIMIT {
            bot.send_message(chat_id, &buf).await?;
            buf.clear();
        }
        if !buf.is_empty() {
            buf.push('\n');
        }
        buf.push_str(line);
    }
    if !buf.is_empty() {
        bot.send_message(chat_id, &buf).await?;
    }
    Ok(())
}
