//! Привязка группы (my_chat_member) и трекинг вступлений (chat_member).
//! Specs: group-binding, join-tracking.

use std::sync::Arc;

use anyhow::Result;
use teloxide::prelude::*;
use teloxide::types::{ChatId, ChatMemberKind, ChatMemberUpdated};

use crate::{db, App};

const INSTRUCTION_TEXT: &str = "Для работы назначьте бота администратором \
                                с правом «Invite Users» (Пригласительные ссылки).";
const ALREADY_BOUND_TEXT: &str = "Бот уже обслуживает другую группу и покидает эту.";

/// Статус бота в группе по событию my_chat_member.
fn bot_can_invite(kind: &ChatMemberKind) -> bool {
    match kind {
        ChatMemberKind::Administrator(a) => a.can_invite_users,
        _ => false,
    }
}

/// cancel-safe: NO — между try_bind и send_message отмена оставит привязку без
/// подтверждения в группе; состояние БД при этом корректно (привязка выполнена).
pub async fn on_my_chat_member(bot: Bot, app: Arc<App>, upd: ChatMemberUpdated) -> Result<()> {
    let chat = &upd.chat;
    // my_chat_member приходит и для личных чатов (block/unblock) — игнорируем.
    if !(chat.is_group() || chat.is_supergroup()) {
        return Ok(());
    }
    let title = chat.title().unwrap_or("").to_string();
    let can_invite = bot_can_invite(&upd.new_chat_member.kind);
    let present = upd.new_chat_member.kind.is_present();
    let active = db::active_chat(&app.pool).await?;

    if can_invite {
        match db::try_bind(&app.pool, chat.id.0, &title).await? {
            db::BindOutcome::Bound => {
                app.auth.clear();
                tracing::info!(chat = chat.id.0, %title, "группа привязана");
                bot.send_message(
                    chat.id,
                    "Готов к работе: группа привязана. Управление — в личке бота, /start.",
                )
                .await?;
            }
            db::BindOutcome::OtherActive(other) => {
                // Та же группа — try_bind вернул бы Bound; сюда попадает только чужая.
                tracing::info!(
                    chat = chat.id.0,
                    bound = other.tg_chat_id,
                    "вторая группа отклонена"
                );
                bot.send_message(chat.id, ALREADY_BOUND_TEXT).await?;
                bot.leave_chat(chat.id).await?;
            }
        }
        return Ok(());
    }

    // Прав нет. Если это привязанная группа — деактивация (понижение/удаление).
    if let Some(a) = &active {
        if a.tg_chat_id == chat.id.0 {
            db::deactivate_chat(&app.pool, chat.id.0).await?;
            app.auth.clear();
            tracing::warn!(chat = chat.id.0, "права потеряны — привязка деактивирована");
            if present {
                // Бот ещё в группе: просим вернуть права.
                bot.send_message(chat.id, INSTRUCTION_TEXT).await?;
            }
            return Ok(());
        }
        // Чужая группа: добавлен без прав при живой привязке — уходим сразу.
        if present {
            bot.send_message(chat.id, ALREADY_BOUND_TEXT).await?;
            bot.leave_chat(chat.id).await?;
        }
        return Ok(());
    }

    // Привязки нет, добавлен без прав — инструкция и ожидание повышения.
    if present {
        bot.send_message(chat.id, INSTRUCTION_TEXT).await?;
    }
    Ok(())
}

/// cancel-safe: NO — между mark_joined и revoke отмена оставит использованную, но не
/// отозванную ссылку; member_limit=1 ограничивает ущерб, повторное событие по ней
/// не сматчится (used_at уже стоит), ссылку можно отозвать вручную.
pub async fn on_chat_member(bot: Bot, app: Arc<App>, upd: ChatMemberUpdated) -> Result<()> {
    let Some(active) = db::active_chat(&app.pool).await? else {
        return Ok(());
    };
    if upd.chat.id.0 != active.tg_chat_id {
        return Ok(());
    }
    let joined = !upd.old_chat_member.kind.is_present() && upd.new_chat_member.kind.is_present();
    if !joined {
        return Ok(());
    }
    // Вступление не по ссылке (одобрение заявки, добавление админом) — не наш случай.
    let Some(tg_link) = &upd.invite_link else {
        return Ok(());
    };
    // Ссылка не из нашего реестра (например, другого админа) — no-op (spec: join-tracking).
    let Some(link) = db::link_by_url(&app.pool, &tg_link.invite_link).await? else {
        return Ok(());
    };
    if link.used_at.is_some() {
        // Идемпотентность: повторное событие по уже использованной ссылке.
        return Ok(());
    }
    let Some(entry) = db::entry_by_id(&app.pool, link.roster_id).await? else {
        return Ok(());
    };

    let user = &upd.new_chat_member.user;
    let now = db::now_ts();
    db::mark_joined(
        &app.pool,
        entry.id,
        user.id.0 as i64,
        &user.full_name(),
        user.username.as_deref(),
        now,
    )
    .await?;
    db::mark_link_used(&app.pool, link.id, now).await?;
    tracing::info!(
        callsign = %entry.callsign,
        user = user.id.0,
        username = user.username.as_deref().unwrap_or("-"),
        "вступление зафиксировано"
    );
    if entry.username.is_some() && entry.username.as_deref() != user.username.as_deref() {
        tracing::warn!(
            callsign = %entry.callsign,
            expected = entry.username.as_deref().unwrap_or("-"),
            actual = user.username.as_deref().unwrap_or("-"),
            "расхождение username вступившего"
        );
    }

    // Отзыв с ретраями (design.md: revoke — авторитетный механизм одноразовости;
    // member_limit=1 не строго одноразов — выход освобождает слот).
    let mut revoked = false;
    for delay_secs in [0u64, 1, 3] {
        if delay_secs > 0 {
            tokio::time::sleep(std::time::Duration::from_secs(delay_secs)).await;
        }
        match bot
            .revoke_chat_invite_link(ChatId(active.tg_chat_id), link.invite_link.clone())
            .await
        {
            Ok(_) => {
                db::mark_link_revoked(&app.pool, link.id, db::now_ts()).await?;
                revoked = true;
                break;
            }
            Err(e) => {
                // В логи — только id, не URL: живая ссылка в логах = утечка (ревью).
                tracing::warn!(link_id = link.id, error = %e, "revoke не прошёл, ретрай");
            }
        }
    }
    if !revoked {
        tracing::error!(link_id = link.id, callsign = %entry.callsign, "revoke не прошёл после ретраев");
        // Уведомляем оператора, создавшего запись; ошибка доставки осознанно глотается
        // (личка могла быть не открыта) — основной канал уже есть: лог выше.
        let _ = bot
            .send_message(
                ChatId(entry.created_by),
                format!(
                    "⚠ «{}» вступил, но отозвать ссылку не удалось. \
                     Отзовите вручную в настройках группы: {}",
                    entry.callsign, link.invite_link
                ),
            )
            .await;
    }
    Ok(())
}
