# Tasks: add-telegram-invite-bot

## 1. Каркас проекта

- [x] 1.1 `cargo init` (bin `auto-inviter-bot`), зависимости: `teloxide = "0.17"` (features `macros`), `tokio`, `sqlx` (sqlite, runtime-tokio, migrate), `tracing` + `tracing-subscriber`, `dotenvy`, `anyhow`
- [x] 1.2 `main.rs`: чтение `BOT_TOKEN` (fail-fast с понятной ошибкой при отсутствии), `DATABASE_PATH` с дефолтом, инициализация tracing, заглушка Dispatcher
- [x] 1.3 Проверка: `cargo build` зелёный, запуск без токена падает с сообщением

## 2. БД и слой хранения

- [x] 2.1 Миграция `0001_init.sql`: таблицы `chats`, `roster`, `invite_links` по схеме из design.md D5, включая partial unique index на активную ссылку
- [x] 2.2 Модуль `db`: типы записей + функции `bind_chat`, `deactivate_chat`, `active_chat`, `add_entry`, `find_entries(query)`, `list_entries`, `delete_entry`, `mark_joined`, `save_link`, `active_link(roster_id)`, `mark_link_used/revoked`
- [x] 2.3 Integration-тесты слоя `db` на sqlite in-memory (tests first): уникальность позывного без учёта регистра, инвариант одной активной ссылки, статусы-производные

## 3. Парсер bulk-импорта (TDD)

- [x] 3.1 Unit-тесты парсера: `позывной`, `позывной @username`, разделители `;`/`,`/таб, строка без позывного (`@username` only) → ошибка с номером строки, пустые строки игнорируются
- [x] 3.2 Реализация чистой функции `parse_roster_lines(&str) -> Vec<Result<Entry, LineError>>` до зелёных тестов

## 4. Привязка группы (group-binding)

- [x] 4.1 Хендлер `my_chat_member`: повышение до admin+can_invite_users → bind + подтверждение в группу; добавление без прав → инструкция; вторая группа при живой привязке → сообщение + `leaveChat`; понижение/удаление → деактивация привязки
- [x] 4.2 Логирование фактического набора `allowed_updates` при старте (должен содержать `chat_member`, `my_chat_member`)

## 5. Контроль доступа (operator-access-control)

- [x] 5.1 Модуль `auth`: `is_operator(user_id)` через `getChatMember` привязанной группы (`creator`/`administrator`), кэш HashMap+Instant с TTL 60 с
- [x] 5.2 Guard-фильтры в dptree: команды/тексты в личке и inline-запросы проходят проверку; не-оператор: команда → отказ, текст → игнор, inline → пустой ответ; нет привязки → инструкция по привязке

## 6. Реестр: команды (roster-management)

- [x] 6.1 `/add <позывной> [@username]` с обработкой дубликата (показ существующей записи и статуса)
- [x] 6.2 Bulk-импорт: некомандный текст оператора в личке → `parse_roster_lines` → вставка → сводка (добавлено / дубликаты / битые строки с номерами)
- [x] 6.3 `/list`: позывной, username, статус (⏳/🔗/✅), для вступивших — дата и фактический username (+пометка расхождения); разбивка длинного вывода на несколько сообщений
- [x] 6.4 `/del <позывной>`: отзыв активной ссылки + удаление записи
- [x] 6.5 `/start` и `/status`: справка; привязанная группа, права бота, счётчики записей по статусам

## 7. Invite-ссылки (invite-link-issuance)

- [x] 7.1 Модуль `links`: `ensure_link(roster_id)` — get-or-create с reuse активной; создание через `createChatInviteLink(member_limit=1, name=позывной[..32])`, сохранение в БД
- [x] 7.2 Eager-создание ссылки в `/add` и bulk-импорте при привязанной группе (со статусом «выдана» в ответе)
- [x] 7.3 `/reissue <позывной>`: отзыв активной (если есть) + новая ссылка, работает и для вступивших
- [x] 7.4 Обработка ошибок Bot API (нет прав, группа не привязана) → понятные сообщения оператору

## 8. Inline-выдача (inline-link-delivery)

- [x] 8.1 Хендлер `InlineQuery`: substring-поиск по позывному/username (case-insensitive), пустой запрос → до 10 невступивших; ≤10 результатов
- [x] 8.2 `InlineQueryResultArticle`: title=позывной, description=username+статус; невступившие → message с персональной ссылкой (`ensure_link`); вступившие → message «уже в группе» без ссылки; `is_personal=true`, `cache_time≤5`
- [x] 8.3 Не-оператор → пустой ответ (через guard из 5.2)

## 9. Трекинг вступлений (join-tracking)

- [x] 9.1 Хендлер `chat_member` привязанной группы: переход в участники + `invite_link` совпадает с активной ссылкой → `mark_joined` (user_id, фактический username, время) + `mark_link_used` + `revokeChatInviteLink`
- [x] 9.2 Вступление по неизвестной ссылке → no-op; расхождение username реестра и фактического — сохранить оба (видно в `/list`)

## 10. Docker и поставка (deployment)

- [x] 10.1 Multi-stage `Dockerfile`: rust:alpine (musl static) → минимальный рантайм (scratch+CA или alpine); проверить размер образа
- [x] 10.2 `docker-compose.yml`: `restart: unless-stopped`, `env_file: .env`, volume `./data:/data`, без портов; `.env.example` с одним `BOT_TOKEN`
- [x] 10.3 `README.md`: BotFather-настройка (`/setinline`, опционально `/setinlinefeedback`), добавление в группу админом с «Invite Users», команды бота, деплой
- [x] 10.4 Проверка: `docker compose up -d --build` локально, бот стартует с чистым volume, состояние переживает пересоздание контейнера

## 13. Quick-add в inline

- [x] 13.1 Unit-тесты классификатора запроса: пустой/поиск/`X+`/`X @user+`/`@user+`→ошибка/`+` отдельно/лишние токены
- [x] 13.2 Реализация: classify_query, ветка Create (add_entry→ensure_link, идемпотентность), карточка-подсказка при пустом поиске и при ошибке разбора; обновить /start и README
- [x] 13.3 `cargo test` + clippy зелёные; redeploy на сервер
- [x] 13.4 Карточный флоу вместо суффикса (фидбек оператора): карточки «➕ Создать „X"» (при отсутствии точного совпадения) и «➕ Новая ссылка» (первая при пустом запросе, авто-позывной «Гость-N»); заглушка с inline-клавиатурой → `chosen_inline_result` → `editMessageText(inline_message_id)`; убрать «+»-синтаксис; тесты valid_quick и нумерации гостей
- [x] 13.5 README + /start: `/setinlinefeedback`=Enabled обязателен; `cargo test`/clippy зелёные; redeploy на сервер

## 14. Личность вступившего и нейтральный текст приглашения

- [x] 14.1 Миграция `0002_joined_name.sql` (+ колонка `roster.joined_name`); `mark_joined` сохраняет имя и фамилию; тесты db обновлены
- [x] 14.2 Нейтральный текст приглашения без позывного/«Гость-N» (build_result и on_chosen)
- [x] 14.3 «Имя Фамилия (@username)» для вступивших в `/list` и описаниях inline-карточек
- [x] 14.4 `cargo test` + clippy зелёные; redeploy на сервер

## 11. E2E-верификация с тестовым ботом (вручную, по спекам)

- [ ] 11.1 Привязка: добавить бота в тестовую группу без прав (инструкция) → повысить (привязка); вторая группа → отказ+выход
- [ ] 11.2 Реестр: `/add`, bulk-список (с дубликатом и битой строкой), `/list`, `/del`
- [ ] 11.3 Inline: поиск с другого аккаунта-админа, выдача ссылки в личный чат, пустой ответ для не-оператора
- [ ] 11.4 Вступление: войти по персональной ссылке третьим аккаунтом → статус «вступил», ссылка отозвана; выйти и попытаться войти по той же ссылке повторно → отклонено (revoke сработал); **подтвердить, что имя-позывной видно в «Invite Links» группы** (open question из design.md)
- [x] 11.5 `cargo test` зелёный; `cargo clippy --all-targets` чистый

## 12. Деплой на сервер

- [x] 12.1 rsync проекта на `sobieg@10.216.0.11:~/full_server/auto-inviter-bot`, `.env` с боевым токеном (не коммитить)
- [x] 12.2 `docker compose up -d --build` на сервере, проверить логи (`docker compose logs -f`) — long polling запущен
- [ ] 12.3 Боевая группа: добавить бота админом с «Invite Users», smoke-тест inline-выдачи
