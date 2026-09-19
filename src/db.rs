//! Слой хранения: sqlx + SQLite. Все времена — unix epoch (секунды, i64).

use anyhow::Result;
use sqlx::{
    sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions},
    SqlitePool,
};

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct BoundChat {
    pub id: i64,
    pub tg_chat_id: i64,
    pub title: String,
    pub is_active: bool,
}

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct Entry {
    pub id: i64,
    pub callsign: String,
    pub username: Option<String>,
    pub created_by: i64,
    pub joined_name: Option<String>,
    pub joined_username: Option<String>,
    pub joined_at: Option<i64>,
}

/// Запись для /list. URL активной ссылки наружу не отдаём (need-to-know):
/// каналы выдачи — inline и /reissue.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct ListedEntry {
    pub callsign: String,
    pub username: Option<String>,
    pub joined_name: Option<String>,
    pub joined_username: Option<String>,
    pub joined_at: Option<i64>,
    pub has_active_link: bool,
}

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct Link {
    pub id: i64,
    pub roster_id: i64,
    pub invite_link: String,
    pub used_at: Option<i64>,
}

#[derive(Debug)]
pub enum AddOutcome {
    Added(Entry),
    Duplicate(Entry),
}

#[derive(Debug)]
pub enum BindOutcome {
    Bound,
    OtherActive(BoundChat),
}

pub struct Counts {
    pub total: i64,
    pub joined: i64,
    pub issued: i64,
}

impl Counts {
    pub fn pending(&self) -> i64 {
        self.total - self.joined - self.issued
    }
}

const ENTRY_COLS: &str =
    "id, callsign, username, created_by, joined_name, joined_username, joined_at";
const LINK_COLS: &str = "id, roster_id, invite_link, used_at";

pub fn now_ts() -> i64 {
    // unwrap: системное время после UNIX_EPOCH — статически невозможная ошибка на живой системе
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64
}

pub async fn connect(path: &str) -> Result<SqlitePool> {
    let opts = SqliteConnectOptions::new()
        .filename(path)
        .create_if_missing(true)
        .journal_mode(SqliteJournalMode::Wal)
        .foreign_keys(true);
    let pool = SqlitePoolOptions::new()
        .max_connections(5)
        .connect_with(opts)
        .await?;
    sqlx::migrate!("./migrations").run(&pool).await?;
    Ok(pool)
}

// --- chats ---

pub async fn active_chat(pool: &SqlitePool) -> Result<Option<BoundChat>> {
    let chat = sqlx::query_as::<_, BoundChat>(
        "SELECT id, tg_chat_id, title, is_active FROM chats WHERE is_active = 1 LIMIT 1",
    )
    .fetch_optional(pool)
    .await?;
    Ok(chat)
}

/// Атомарная привязка: успех только если нет другой активной группы.
pub async fn try_bind(pool: &SqlitePool, tg_chat_id: i64, title: &str) -> Result<BindOutcome> {
    let mut tx = pool.begin().await?;
    let other = sqlx::query_as::<_, BoundChat>(
        "SELECT id, tg_chat_id, title, is_active FROM chats \
         WHERE is_active = 1 AND tg_chat_id != ? LIMIT 1",
    )
    .bind(tg_chat_id)
    .fetch_optional(&mut *tx)
    .await?;
    if let Some(o) = other {
        tx.rollback().await?;
        return Ok(BindOutcome::OtherActive(o));
    }
    sqlx::query(
        "INSERT INTO chats (tg_chat_id, title, is_active, bound_at) VALUES (?, ?, 1, ?) \
         ON CONFLICT(tg_chat_id) DO UPDATE SET title = excluded.title, is_active = 1, bound_at = excluded.bound_at",
    )
    .bind(tg_chat_id)
    .bind(title)
    .bind(now_ts())
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(BindOutcome::Bound)
}

pub async fn deactivate_chat(pool: &SqlitePool, tg_chat_id: i64) -> Result<()> {
    sqlx::query("UPDATE chats SET is_active = 0 WHERE tg_chat_id = ?")
        .bind(tg_chat_id)
        .execute(pool)
        .await?;
    Ok(())
}

// --- roster ---

pub async fn entry_by_callsign(
    pool: &SqlitePool,
    chat_id: i64,
    callsign: &str,
) -> Result<Option<Entry>> {
    let entry = sqlx::query_as::<_, Entry>(&format!(
        "SELECT {ENTRY_COLS} FROM roster WHERE chat_id = ? AND callsign_norm = ?"
    ))
    .bind(chat_id)
    .bind(callsign.to_lowercase())
    .fetch_optional(pool)
    .await?;
    Ok(entry)
}

pub async fn add_entry(
    pool: &SqlitePool,
    chat_id: i64,
    callsign: &str,
    username: Option<&str>,
    created_by: i64,
) -> Result<AddOutcome> {
    if let Some(existing) = entry_by_callsign(pool, chat_id, callsign).await? {
        return Ok(AddOutcome::Duplicate(existing));
    }
    let res = sqlx::query(
        "INSERT INTO roster (chat_id, callsign, callsign_norm, username, created_by, created_at) \
         VALUES (?, ?, ?, ?, ?, ?)",
    )
    .bind(chat_id)
    .bind(callsign)
    .bind(callsign.to_lowercase())
    .bind(username)
    .bind(created_by)
    .bind(now_ts())
    .execute(pool)
    .await;
    match res {
        Ok(done) => {
            let entry = sqlx::query_as::<_, Entry>(&format!(
                "SELECT {ENTRY_COLS} FROM roster WHERE id = ?"
            ))
            .bind(done.last_insert_rowid())
            .fetch_one(pool)
            .await?;
            Ok(AddOutcome::Added(entry))
        }
        // Гонка двух одновременных добавлений (§B13): unique-индекс — backstop,
        // проигравший перечитывает существующую запись.
        Err(e) if is_unique_violation(&e) => {
            let existing = entry_by_callsign(pool, chat_id, callsign)
                .await?
                .ok_or_else(|| anyhow::anyhow!("unique violation, но запись не найдена"))?;
            Ok(AddOutcome::Duplicate(existing))
        }
        Err(e) => Err(e.into()),
    }
}

/// Создаёт запись с автоматическим позывным «Гость-N» (наименьший свободный,
/// начиная с числа существующих гостей + 1). Для карточки «Новая ссылка».
pub async fn create_guest_entry(pool: &SqlitePool, chat_id: i64, created_by: i64) -> Result<Entry> {
    let (guests,): (i64,) = sqlx::query_as(
        "SELECT COUNT(*) FROM roster WHERE chat_id = ? AND callsign_norm LIKE 'гость-%'",
    )
    .bind(chat_id)
    .fetch_one(pool)
    .await?;
    // Цикл закрывает дыры в нумерации и гонки: Duplicate → следующий номер.
    let start = guests + 1;
    for n in start..start + 1000 {
        match add_entry(pool, chat_id, &format!("Гость-{n}"), None, created_by).await? {
            AddOutcome::Added(e) => return Ok(e),
            AddOutcome::Duplicate(_) => continue,
        }
    }
    anyhow::bail!("не удалось подобрать свободный номер гостя")
}

fn is_unique_violation(e: &sqlx::Error) -> bool {
    e.as_database_error()
        .map(|d| d.is_unique_violation())
        .unwrap_or(false)
}

/// Экранирует %, _ и \ для LIKE ... ESCAPE '\'.
pub fn escape_like(s: &str) -> String {
    s.replace('\\', "\\\\").replace('%', "\\%").replace('_', "\\_")
}

pub async fn find_entries(
    pool: &SqlitePool,
    chat_id: i64,
    query: &str,
    limit: i64,
) -> Result<Vec<Entry>> {
    let pattern = format!("%{}%", escape_like(&query.to_lowercase()));
    let entries = sqlx::query_as::<_, Entry>(&format!(
        "SELECT {ENTRY_COLS} FROM roster \
         WHERE chat_id = ? AND (callsign_norm LIKE ? ESCAPE '\\' OR username LIKE ? ESCAPE '\\') \
         ORDER BY (joined_at IS NOT NULL), callsign_norm LIMIT ?"
    ))
    .bind(chat_id)
    .bind(&pattern)
    .bind(&pattern)
    .bind(limit)
    .fetch_all(pool)
    .await?;
    Ok(entries)
}

pub async fn pending_entries(pool: &SqlitePool, chat_id: i64, limit: i64) -> Result<Vec<Entry>> {
    let entries = sqlx::query_as::<_, Entry>(&format!(
        "SELECT {ENTRY_COLS} FROM roster \
         WHERE chat_id = ? AND joined_at IS NULL ORDER BY callsign_norm LIMIT ?"
    ))
    .bind(chat_id)
    .bind(limit)
    .fetch_all(pool)
    .await?;
    Ok(entries)
}

pub async fn list_entries(pool: &SqlitePool, chat_id: i64) -> Result<Vec<ListedEntry>> {
    let entries = sqlx::query_as::<_, ListedEntry>(
        "SELECT r.callsign, r.username, r.joined_name, r.joined_username, r.joined_at, \
                EXISTS(SELECT 1 FROM invite_links l WHERE l.roster_id = r.id \
                       AND l.revoked_at IS NULL AND l.used_at IS NULL) AS has_active_link \
         FROM roster r WHERE r.chat_id = ? ORDER BY r.callsign_norm",
    )
    .bind(chat_id)
    .fetch_all(pool)
    .await?;
    Ok(entries)
}

pub async fn delete_entry(pool: &SqlitePool, entry_id: i64) -> Result<()> {
    sqlx::query("DELETE FROM roster WHERE id = ?")
        .bind(entry_id)
        .execute(pool)
        .await?;
    Ok(())
}

/// Помечает запись вступившей; первое вступление выигрывает (повтор — no-op).
pub async fn mark_joined(
    pool: &SqlitePool,
    entry_id: i64,
    user_id: i64,
    full_name: &str,
    username: Option<&str>,
    ts: i64,
) -> Result<()> {
    sqlx::query(
        "UPDATE roster SET joined_tg_user_id = ?, joined_name = ?, joined_username = ?, joined_at = ? \
         WHERE id = ? AND joined_at IS NULL",
    )
    .bind(user_id)
    .bind(full_name)
    .bind(username)
    .bind(ts)
    .bind(entry_id)
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn counts(pool: &SqlitePool, chat_id: i64) -> Result<Counts> {
    let (total, joined, issued): (i64, i64, i64) = sqlx::query_as(
        "SELECT COUNT(*), \
                COALESCE(SUM(CASE WHEN joined_at IS NOT NULL THEN 1 ELSE 0 END), 0), \
                COALESCE(SUM(CASE WHEN joined_at IS NULL AND EXISTS( \
                    SELECT 1 FROM invite_links l WHERE l.roster_id = r.id \
                    AND l.revoked_at IS NULL AND l.used_at IS NULL) THEN 1 ELSE 0 END), 0) \
         FROM roster r WHERE chat_id = ?",
    )
    .bind(chat_id)
    .fetch_one(pool)
    .await?;
    Ok(Counts { total, joined, issued })
}

// --- invite_links ---

pub async fn active_link(pool: &SqlitePool, roster_id: i64) -> Result<Option<Link>> {
    let link = sqlx::query_as::<_, Link>(&format!(
        "SELECT {LINK_COLS} FROM invite_links \
         WHERE roster_id = ? AND revoked_at IS NULL AND used_at IS NULL"
    ))
    .bind(roster_id)
    .fetch_optional(pool)
    .await?;
    Ok(link)
}

pub async fn save_link(
    pool: &SqlitePool,
    roster_id: i64,
    invite_link: &str,
    name: &str,
) -> Result<Link> {
    let res = sqlx::query(
        "INSERT INTO invite_links (roster_id, invite_link, name, created_at) VALUES (?, ?, ?, ?)",
    )
    .bind(roster_id)
    .bind(invite_link)
    .bind(name)
    .bind(now_ts())
    .execute(pool)
    .await;
    match res {
        Ok(done) => {
            let link = sqlx::query_as::<_, Link>(&format!(
                "SELECT {LINK_COLS} FROM invite_links WHERE id = ?"
            ))
            .bind(done.last_insert_rowid())
            .fetch_one(pool)
            .await?;
            Ok(link)
        }
        // Партиальный unique-индекс сработал: активная ссылка уже есть — возвращаем её.
        Err(e) if is_unique_violation(&e) => active_link(pool, roster_id)
            .await?
            .ok_or_else(|| anyhow::anyhow!("unique violation, но активной ссылки нет")),
        Err(e) => Err(e.into()),
    }
}

pub async fn link_by_url(pool: &SqlitePool, url: &str) -> Result<Option<Link>> {
    let link = sqlx::query_as::<_, Link>(&format!(
        "SELECT {LINK_COLS} FROM invite_links WHERE invite_link = ?"
    ))
    .bind(url)
    .fetch_optional(pool)
    .await?;
    Ok(link)
}

pub async fn entry_by_id(pool: &SqlitePool, entry_id: i64) -> Result<Option<Entry>> {
    let entry = sqlx::query_as::<_, Entry>(&format!(
        "SELECT {ENTRY_COLS} FROM roster WHERE id = ?"
    ))
    .bind(entry_id)
    .fetch_optional(pool)
    .await?;
    Ok(entry)
}

pub async fn mark_link_used(pool: &SqlitePool, link_id: i64, ts: i64) -> Result<()> {
    sqlx::query("UPDATE invite_links SET used_at = ? WHERE id = ? AND used_at IS NULL")
        .bind(ts)
        .bind(link_id)
        .execute(pool)
        .await?;
    Ok(())
}

pub async fn mark_link_revoked(pool: &SqlitePool, link_id: i64, ts: i64) -> Result<()> {
    sqlx::query("UPDATE invite_links SET revoked_at = ? WHERE id = ? AND revoked_at IS NULL")
        .bind(ts)
        .bind(link_id)
        .execute(pool)
        .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn test_pool() -> SqlitePool {
        // max_connections(1): у :memory: каждая коннекция — отдельная БД
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap();
        sqlx::migrate!("./migrations").run(&pool).await.unwrap();
        pool
    }

    async fn bound(pool: &SqlitePool) -> BoundChat {
        match try_bind(pool, -100123, "Тестовая группа").await.unwrap() {
            BindOutcome::Bound => active_chat(pool).await.unwrap().expect("привязка есть"),
            BindOutcome::OtherActive(_) => panic!("ожидалась привязка"),
        }
    }

    #[tokio::test]
    async fn callsign_unique_case_insensitive_cyrillic() {
        let pool = test_pool().await;
        let chat = bound(&pool).await;
        let out = add_entry(&pool, chat.id, "Сокол", Some("ivan"), 1).await.unwrap();
        assert!(matches!(out, AddOutcome::Added(_)));
        // кириллица в другом регистре — дубликат (NOCASE SQLite этого не умеет, наша нормализация — умеет)
        let out = add_entry(&pool, chat.id, "СОКОЛ", None, 1).await.unwrap();
        match out {
            AddOutcome::Duplicate(e) => assert_eq!(e.callsign, "Сокол"),
            other => panic!("ожидался Duplicate, получено {other:?}"),
        }
    }

    #[tokio::test]
    async fn one_active_link_invariant() {
        let pool = test_pool().await;
        let chat = bound(&pool).await;
        let AddOutcome::Added(entry) = add_entry(&pool, chat.id, "Беркут", None, 1).await.unwrap()
        else {
            panic!()
        };
        let l1 = save_link(&pool, entry.id, "https://t.me/+aaa", "Беркут").await.unwrap();
        // вторая вставка при живой активной — возвращает существующую, не создаёт новую
        let l2 = save_link(&pool, entry.id, "https://t.me/+bbb", "Беркут").await.unwrap();
        assert_eq!(l1.id, l2.id);
        assert_eq!(l2.invite_link, "https://t.me/+aaa");
        // после use активной нет, новая создаётся
        mark_link_used(&pool, l1.id, now_ts()).await.unwrap();
        assert!(active_link(&pool, entry.id).await.unwrap().is_none());
        let l3 = save_link(&pool, entry.id, "https://t.me/+ccc", "Беркут").await.unwrap();
        assert_ne!(l3.id, l1.id);
    }

    #[tokio::test]
    async fn derived_statuses() {
        let pool = test_pool().await;
        let chat = bound(&pool).await;
        let AddOutcome::Added(e1) = add_entry(&pool, chat.id, "Ястреб", None, 1).await.unwrap()
        else {
            panic!()
        };
        let AddOutcome::Added(e2) =
            add_entry(&pool, chat.id, "Сокол", Some("ivan"), 1).await.unwrap()
        else {
            panic!()
        };
        save_link(&pool, e2.id, "https://t.me/+xxx", "Сокол").await.unwrap();

        let c = counts(&pool, chat.id).await.unwrap();
        assert_eq!((c.total, c.joined, c.issued, c.pending()), (2, 0, 1, 1));

        // e2 вступает (с другим username — расхождение фиксируется)
        let link = active_link(&pool, e2.id).await.unwrap().unwrap();
        mark_joined(&pool, e2.id, 555, "Пётр Иванов", Some("someone_else"), now_ts())
            .await
            .unwrap();
        mark_link_used(&pool, link.id, now_ts()).await.unwrap();

        let c = counts(&pool, chat.id).await.unwrap();
        assert_eq!((c.total, c.joined, c.issued, c.pending()), (2, 1, 0, 1));

        let listed = list_entries(&pool, chat.id).await.unwrap();
        let sokol = listed.iter().find(|l| l.callsign == "Сокол").unwrap();
        assert_eq!(sokol.joined_name.as_deref(), Some("Пётр Иванов"));
        assert_eq!(sokol.joined_username.as_deref(), Some("someone_else"));
        assert_eq!(sokol.username.as_deref(), Some("ivan"));
        assert!(!sokol.has_active_link);
        let yastreb = listed.iter().find(|l| l.callsign == "Ястреб").unwrap();
        assert!(yastreb.joined_at.is_none() && !yastreb.has_active_link);
        let _ = e1;
    }

    #[tokio::test]
    async fn mark_joined_first_wins() {
        let pool = test_pool().await;
        let chat = bound(&pool).await;
        let AddOutcome::Added(e) = add_entry(&pool, chat.id, "Гриф", None, 1).await.unwrap()
        else {
            panic!()
        };
        mark_joined(&pool, e.id, 111, "Первый", Some("first"), 1000).await.unwrap();
        mark_joined(&pool, e.id, 222, "Второй", Some("second"), 2000).await.unwrap();
        // user_id хранится только в БД (production-структуры его не носят) — читаем напрямую
        let (user_id, joined_at): (i64, i64) =
            sqlx::query_as("SELECT joined_tg_user_id, joined_at FROM roster WHERE id = ?")
                .bind(e.id)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!((user_id, joined_at), (111, 1000));
    }

    #[tokio::test]
    async fn second_group_rejected_then_rebind_after_deactivate() {
        let pool = test_pool().await;
        let first = bound(&pool).await;
        match try_bind(&pool, -200456, "Вторая").await.unwrap() {
            BindOutcome::OtherActive(o) => assert_eq!(o.tg_chat_id, first.tg_chat_id),
            BindOutcome::Bound => panic!("вторая группа не должна привязаться"),
        }
        deactivate_chat(&pool, first.tg_chat_id).await.unwrap();
        assert!(active_chat(&pool).await.unwrap().is_none());
        // реактивация той же группы
        match try_bind(&pool, first.tg_chat_id, "Тестовая группа").await.unwrap() {
            BindOutcome::Bound => {
                assert!(active_chat(&pool).await.unwrap().unwrap().is_active)
            }
            BindOutcome::OtherActive(_) => panic!(),
        }
        // данные реестра пережили деактивацию
        let c = counts(&pool, first.id).await.unwrap();
        assert_eq!(c.total, 0);
    }

    #[tokio::test]
    async fn find_entries_substring_and_username() {
        let pool = test_pool().await;
        let chat = bound(&pool).await;
        add_entry(&pool, chat.id, "Сокол", Some("ivan_petrov"), 1).await.unwrap();
        add_entry(&pool, chat.id, "Высокий", None, 1).await.unwrap();
        add_entry(&pool, chat.id, "Беркут", Some("petr"), 1).await.unwrap();

        // подстрока в позывном, регистронезависимо (кириллица)
        let found = find_entries(&pool, chat.id, "сок", 10).await.unwrap();
        let names: Vec<_> = found.iter().map(|e| e.callsign.as_str()).collect();
        assert_eq!(names, vec!["Высокий", "Сокол"]);

        // по username
        let found = find_entries(&pool, chat.id, "petro", 10).await.unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].callsign, "Сокол");

        // LIKE-спецсимволы не ломают поиск
        let found = find_entries(&pool, chat.id, "100%", 10).await.unwrap();
        assert!(found.is_empty());
    }

    #[test]
    fn escape_like_handles_specials() {
        assert_eq!(escape_like("a%b_c\\d"), "a\\%b\\_c\\\\d");
    }

    #[tokio::test]
    async fn guest_numbering_sequential_and_skips_taken() {
        let pool = test_pool().await;
        let chat = bound(&pool).await;
        let g1 = create_guest_entry(&pool, chat.id, 1).await.unwrap();
        assert_eq!(g1.callsign, "Гость-1");
        let g2 = create_guest_entry(&pool, chat.id, 1).await.unwrap();
        assert_eq!(g2.callsign, "Гость-2");
        // занятый вручную номер (в другом регистре) пропускается
        add_entry(&pool, chat.id, "гость-3", None, 1).await.unwrap();
        let g4 = create_guest_entry(&pool, chat.id, 1).await.unwrap();
        assert_eq!(g4.callsign, "Гость-4");
    }
}
