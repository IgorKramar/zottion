# CLI zottion v0.1

Автор: Igor Kramar. Общие флаги можно указывать до и после подкоманды:
`--json`, `--profile NAME`, `--workspace ID`, `--timeout SECONDS`, `--offline`.
Справка конкретной команды: `zottion COMMAND --help`.

| Группа | Команды |
|---|---|
| auth | login --server URL [--no-browser], status, refresh, logout [--revoke] |
| page | get [--content], create --parent ID или --data-source ID, title, move --parent ID, trash --yes, restore, favorite, pin, recent |
| page property | get --property ID [--all], set --property NAME_OR_ID --value-json JSON или --file FILE |
| block | get, children, update --text/--file/--patch-file, append --kind TYPE --text/--file или --blocks-file, format, check --checked true/false, trash, restore, move (явный код 7) |
| data-source | get, query --file BODY или --filter-file FILTER и --sort-file SORTS |
| view | get, list --database ID, query |
| search | QUERY --scope titles/local |
| draft | list, show, apply, resolve --choice local/remote, drop --yes, clear-cache |
| operation | list, show, check, resolve --confirmed true/false --yes |
| completions | bash, zsh, fish, powershell, elvish |

`page create --file` принимает обычный текст, разделённый пустыми строками на
paragraph. `block update --file` принимает текст одного блока; `--patch-file`
и `append --blocks-file` — JSON в формате изменяемых полей Notion. Число
блоков одного append ограничено 100. Неизвестные типы не создаются и не меняются.

`--file -`, а также `--patch-file -` и другие файловые параметры читают stdin.
Текст должен быть UTF-8, максимальный вход — 10 MiB. Один поток stdin нельзя
использовать сразу для двух файлов одного запроса.

Списки имеют `--cursor` и `--all` (view query создаёт запрос, поэтому для него
курсор содержится в результате, а `--all` дочитывает тот же серверный query ID).
По умолчанию возвращается первая страница и её курсор. Неполный результат из-за
сбоя, повторяющегося курсора или серверного усечения возвращает код 8.
`page get --content` читает непосредственно вложенные блоки; дальнейшая
вложенность загружается через `block children`, в TUI — через `l`.

JSON-результат: `{"schema_version":1,"data":...,"meta":{"profile":...,"workspace":...,"offline":false,"complete":true,"partial":false,"next_cursor":null}}`.
stdout содержит результат; stderr — диагностику. При частичном результате
stdout содержит доступные данные с `partial:true`, stderr — JSON-ошибку.
`--json` не запускает TUI. Обычная команда скрипта не запускает OAuth-вход.

`block format ID --attribute bold --value true --start 0 --end 6` форматирует
диапазон. Позиции считают Unicode-символы Rust (`char`), а не байты; составные
emoji могут состоять из нескольких таких символов. Без end — до конца блока.
Поддержаны bold, italic, underline, strikethrough, code, color и link.
Упоминание нельзя разделить или заменить ссылкой.

При простом изменении текста сохраняются оформление и ссылки затронутого
фрагмента. Правка через границы нескольких rich-text фрагментов отклоняется
вместо молчаливой потери атрибутов; используйте явный JSON PATCH.
Исходный текст с небезопасными управляющими символами TUI показывает как
защищённый JSON, сохраняя содержимое. В таблице
сложный rich text редактируется как JSON. Formula/rollup доступны для чтения.
Новые варианты select/multi-select/status отклоняются, чтобы не менять схему.

| Код | Значение |
|---|---|
| 0 | Успех; офлайн-результат явно содержит published:false |
| 1 | Общая ошибка |
| 2 | Команда или входные данные |
| 3 | Вход/токены |
| 4 | Ресурс недоступен |
| 5 | Конфликт с удалённой версией или другим локальным черновиком |
| 6 | Сеть, лимит или срок запроса |
| 7 | Неподдерживаемая операция |
| 8 | Частичный результат, серверное задание или неизвестный исход записи |
| 130 | Отмена |

Запись с кодом 8 не повторяется автоматически. Для `operation resolve` нужен
результат ручной проверки, а не предположение по времени ожидания.
