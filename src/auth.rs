//! Контроль доступа: оператор = админ привязанной группы (design.md D4).

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use anyhow::Result;
use teloxide::prelude::*;
use teloxide::types::{ChatId, ChatMemberKind, UserId};

use crate::{db, App};

const CACHE_TTL: Duration = Duration::from_secs(60);

#[derive(Debug)]
pub enum Access {
    /// Нет активной привязанной группы — операторская проверка невозможна.
    NoBinding,
    Operator(db::BoundChat),
    Denied,
}

/// Кэш `user_id -> является ли оператором`. Guard никогда не пересекает .await (§B2).
pub struct AuthCache {
    inner: Mutex<HashMap<u64, (bool, Instant)>>,
}

impl AuthCache {
    pub fn new() -> Self {
        Self { inner: Mutex::new(HashMap::new()) }
    }

    fn get(&self, user_id: u64) -> Option<bool> {
        let guard = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        guard
            .get(&user_id)
            .filter(|(_, at)| at.elapsed() < CACHE_TTL)
            .map(|(is_op, _)| *is_op)
    }

    fn put(&self, user_id: u64, is_op: bool) {
        let mut guard = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        guard.insert(user_id, (is_op, Instant::now()));
    }

    /// Сбрасывается при смене привязки группы.
    pub fn clear(&self) {
        let mut guard = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        guard.clear();
    }
}

/// cancel-safe: yes — один read-only Bot API вызов + запись в кэш, частичного состояния нет.
pub async fn check(bot: &Bot, app: &App, user_id: UserId) -> Result<Access> {
    let Some(chat) = db::active_chat(&app.pool).await? else {
        return Ok(Access::NoBinding);
    };
    if let Some(is_op) = app.auth.get(user_id.0) {
        return Ok(if is_op { Access::Operator(chat) } else { Access::Denied });
    }
    // Ошибка getChatMember (пользователь неизвестен чату и т.п.) трактуется как «не оператор»:
    // отказ в доступе безопаснее, чем падение хендлера.
    let is_op = match bot.get_chat_member(ChatId(chat.tg_chat_id), user_id).await {
        Ok(member) => matches!(
            member.kind,
            ChatMemberKind::Owner(_) | ChatMemberKind::Administrator(_)
        ),
        Err(e) => {
            tracing::debug!(user = user_id.0, error = %e, "getChatMember не прошёл — считаем не-оператором");
            false
        }
    };
    app.auth.put(user_id.0, is_op);
    Ok(if is_op { Access::Operator(chat) } else { Access::Denied })
}
