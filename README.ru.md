<div align="center">

<img src="assets/icon-zottion.svg" alt="zottion" width="112" height="112" />

# zottion

[English](README.md) · **Русский**

### Notion. В твоём терминале.

Заметки · задачи · Vim-редактор · CLI для скриптов

[![CI](https://github.com/IgorKramar/zottion/actions/workflows/ci.yml/badge.svg)](https://github.com/IgorKramar/zottion/actions/workflows/ci.yml)
![Статус](https://img.shields.io/badge/status-early_development-edc98c?style=flat-square)
[![MIT](https://img.shields.io/badge/license-MIT-86d9b8?style=flat-square)](LICENSE)
![Rust](https://img.shields.io/badge/Rust-1.91-e0af68?style=flat-square&logo=rust&logoColor=white)
![Платформы](https://img.shields.io/badge/platform-Linux_/_macOS-8ebcf0?style=flat-square)

[Установка](#установка-и-запуск) · [Клавиши](#работа) · [Сохранность](#сохранность-и-восстановление) · [Разработка](#проверка-и-сборка)

</div>

![Заметки в zottion](docs/screenshots/notes.png)

**Записать мысль, передвинуть задачу, сохранить черновик — в одном терминале.**

Неофициальный клиент Notion на Rust с TUI и CLI. Работает через официальный
REST API, редактирует блоки по ID и сохраняет черновики на машине пользователя.
Интерфейс и сообщения приложения сейчас на русском; документация доступна на EN/RU.

<details>
<summary><strong>Таблица и доска задач</strong></summary>

![Доска задач в zottion](docs/screenshots/board.png)

Существующая схема data source, столбцы status/select и изменение свойств записи.
Локальные настройки интерфейса не меняют общие views Notion.

</details>

*Снимки получены из настоящего приложения с демонстрационными данными,
офлайн-кэшем и настроенной ANSI-палитрой терминала. Подключение к Notion не использовалось.*

> **Ранняя версия.** Исходный код опубликован; стабильного релиза пока нет.
> Реализация v0.1 прошла локальные проверки. Приёмка настоящего Notion/OAuth,
> native keychain и чистых систем описана в [отчёте](docs/verification.md).

## Установка и запуск

```sh
cargo install --git https://github.com/IgorKramar/zottion --locked zottion
zottion --help
zottion
```

Для локальной разработки:

```sh
git clone https://github.com/IgorKramar/zottion.git
cd zottion
cargo install --path . --locked
```

Без подкоманды TUI запускается только в терминале. Браузерный вход использует
собственный OAuth-сервис:

```sh
zottion --timeout 300 auth login --server https://auth.example.org
zottion auth status --json
zottion open PAGE_ID_OR_NOTION_URL
```

Готовый production-сервис в репозиторий не входит. Запуск своего описан в
[OAuth](docs/oauth.md). Для разработки и CI клиент принимает `NOTION_TOKEN` из
окружения, без записи токена в настройки или argv. Токен окружения имеет
приоритет над keychain. Страницы должны быть доступны этой connection.

## Работа

В документе `j/k` выбирают блок, Enter включает EdTUI, `i` — Insert, `v` — Visual.
Первый Esc возвращает Normal, следующий — выбор блоков. Изменения автоматически
сохраняются **локально**. `:w` отправляет их в Notion, `:q` сохраняет черновики
при выходе, `:wq` отправляет и выходит после подтверждения.

`Ctrl+P` открывает команды, `?` — справку; `Ctrl+W` переключает дерево/документ,
Tab — панель свойств. `l/h` загружают/сворачивают вложенность.

```text
:open PAGE_ID
:append to_do Подготовить выпуск
:new Вложенная заметка
:format bold true
:table DATA_SOURCE_ID
:board DATA_SOURCE_ID Status
:task Проверить CLI
:view VIEW_ID
:search название
:local содержимое
:diff
:drafts
:operations
```

В таблице `h/l` выбирают ячейку, `e` редактирует свойство, Enter открывает запись.
На доске `h/l` выбирают колонку, `j/k` карточку, `H/L` меняют статус.
Структурные команды (`:append`, `:new`, `:task`, `:move`, подтверждённая корзина)
выполняют явно запрошенную операцию сразу. После неё `:refresh` обновляет экран.
Офлайн они оставляют черновик для явного `:apply ID` или CLI `draft apply`.

Темы: `:theme default|dark|light`; `NO_COLOR` отключает цвета. Ширины таблицы
меняются `+/-` или `:width 12..60`. Эти настройки действуют в текущем запуске
и не изменяют общие представления Notion. Nerd Fonts и truecolor не требуются.

```sh
zottion page get PAGE_ID --content --all --json
zottion block update BLOCK_ID --file - --json
zottion page create --data-source SOURCE_ID --title 'Новая задача' --json
zottion page property set PAGE_ID --property Status --value-json '{"status":{"name":"Done"}}' --json
zottion data-source query SOURCE_ID --filter-file filter.json --all --json
zottion search 'архитектура' --scope local --json
zottion --offline draft list --json
```

[CLI и коды завершения](docs/cli.md) · [Покрытие и ограничения API](docs/capabilities.md)
· [Архитектура](ARCHITECTURE.md).

## Сохранность и восстановление

Черновик сохраняется до отправки. Перед записью клиент заново читает изменяемые
поля; конфликт показывает базовую, локальную и удалённую версии. `--yes` не
отключает эту проверку. API не даёт подтверждённого CAS, поэтому остаётся гонка
между проверкой и записью другим клиентом.

После обрыва связи изменяющий запрос автоматически не повторяется. Используйте
`operation list`, `operation show ID` и `operation check ID`. Для создания
сверьте результат в родителе Notion и только затем явно отметьте исход:

```sh
zottion operation resolve OP_ID --confirmed true --yes
# Только после проверки, что операция не выполнилась:
zottion operation resolve OP_ID --confirmed false --yes
zottion draft apply DRAFT_ID
```

`draft show ID` показывает версии; `draft resolve ID --choice local|remote`
фиксирует явный выбор. Remote удаляет локальный черновик. Local заново принимает
удалённую версию за базу и проверяет её перед отправкой локального изменения.
Очистка `draft clear-cache` сохраняет черновики и журнал. Офлайн означает чтение
загруженного и сохранение черновиков; автоматической отправки очереди нет.

SQLite и настройки находятся в системных каталогах `zottion`. Токены OAuth —
одной записью в Secret Service/Keychain. Профиль, workspace и идентичность
учётных данных изолируют снимки, поиск, черновики и операции. Для тестов и
переносимых окружений доступны `ZOTTION_CONFIG_DIR` и `ZOTTION_STATE_DIR`.

## Проверка и сборка

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked
cargo build --release --workspace --locked --target x86_64-unknown-linux-gnu
python3 scripts/package.py --target x86_64-unknown-linux-gnu
```

CI проверяет Linux/macOS и собирает release. Отдельный ручной workflow
`packages` готовит четыре архива и SHA-256 для Linux/macOS x86_64/ARM64,
не создавая GitHub Release и не развёртывая OAuth-сервис. Архивы macOS не
нотарифицированы; Linux-сборки используют glibc системы сборки. Установка на
чистых системах проверяется перед выпуском стабильной версии.


## Участие

[Сообщить об ошибке](https://github.com/IgorKramar/zottion/issues) ·
[Внести изменение](CONTRIBUTING.md) · [Безопасность](SECURITY.md).

## Лицензия

[MIT](LICENSE) · Copyright © 2026 [Igor Kramar](https://github.com/IgorKramar).
