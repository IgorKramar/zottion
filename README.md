<div align="center">

<img src="assets/icon-zottion.svg" alt="zottion" width="112" height="112" />

# zottion

**English** · [Русский](README.ru.md)

### Notion. In your terminal.

Notes · tasks · Vim editing · a scriptable CLI

[![CI](https://github.com/IgorKramar/zottion/actions/workflows/ci.yml/badge.svg)](https://github.com/IgorKramar/zottion/actions/workflows/ci.yml)
![Status](https://img.shields.io/badge/status-early_development-edc98c?style=flat-square)
[![MIT](https://img.shields.io/badge/license-MIT-86d9b8?style=flat-square)](LICENSE)
![Rust](https://img.shields.io/badge/Rust-1.91-e0af68?style=flat-square&logo=rust&logoColor=white)
![Platforms](https://img.shields.io/badge/platform-Linux_/_macOS-8ebcf0?style=flat-square)

[Install](#install) · [Quick start](#quick-start) · [Keys](#keys) · [Safety](#drafts-and-writes) · [Docs](#documentation)

</div>

![Notes in zottion](docs/screenshots/notes.png)

**Write a note, move a task, keep your draft — without leaving the terminal.**

zottion is an unofficial Rust TUI and CLI for Notion. It talks directly to the
official REST API, edits individual blocks by ID, and keeps drafts on your machine.
The application interface and messages are currently in Russian; the README is available in English and Russian.

<details>
<summary><strong>A board for the work behind your notes</strong></summary>

![Task board in zottion](docs/screenshots/board.png)

Use an existing data source, group cards by status/select, and update the underlying
page properties. Local panel and column settings do not modify shared Notion views.

</details>

*Screens captured from the application with synthetic data, an offline cache, and
a custom terminal ANSI palette. No Notion connection was used.*

> **Early development.** The source is public; there is no stable release yet.
> The v0.1 implementation passes local checks. Real Notion/OAuth, native keychain,
> and clean-system acceptance are tracked in the [verification report](docs/verification.md).

## What's inside

| Area | Available |
|:---|:---|
| Notes | Pages, nested navigation, core block types, block-level EdTUI editing |
| Rich text | Links, mentions and formatting preservation; explicit range formatting |
| Tasks | Existing data source schemas, typed properties, tables and status/select boards |
| Views & search | Read existing views; search Notion titles or locally loaded content |
| Local state | Scoped SQLite cache, FTS5 search, durable drafts and an operation journal |
| Automation | Versioned JSON, stdin, cursors, profiles, exit codes and shell completion |
| Authentication | Browser OAuth through a self-hosted service; token pairs in the OS keychain |

Unknown blocks are displayed and protected. Files, comments, schema editing and
other view layouts are outside v0.1. See the [capability matrix](docs/capabilities.md).

## Install

Build from source with **Rust 1.91 or newer** and Git:

```sh
cargo install --git https://github.com/IgorKramar/zottion --locked zottion
zottion --help
```

Cargo usually installs to `~/.cargo/bin`; keep it in `PATH`. Linux and macOS are
the target platforms. There is no published binary release yet.

<details>
<summary>Build a local checkout</summary>

```sh
git clone https://github.com/IgorKramar/zottion.git
cd zottion
cargo install --path . --locked
```

</details>

Minimum terminal size: **80 × 24**. Unicode is supported; Nerd Fonts and truecolor
are optional. Set `NO_COLOR` to disable colors. Themes are `default`, `dark` and
`light`; `:theme dark` changes the current session.

## Quick start

Browser login needs a configured zottion OAuth service and a registered Notion
public connection. There is no bundled production login service.

```sh
zottion --timeout 300 auth login --server https://YOUR_AUTH_HOST
zottion auth status --json
zottion open PAGE_ID_OR_NOTION_URL
```

See [self-hosted OAuth](docs/oauth.md) for the server setup. For development and
CI, set `NOTION_TOKEN` in the environment. It takes precedence over keychain
credentials; tokens are not accepted in command arguments or stored in config files.
Your connection must have access to the pages you use.

### Notes

Select a block with `j/k`, press Enter, then `i` to write. Text changes are saved
locally. `:w` publishes loaded drafts on the current screen; `:q` exits while
retaining unsubmitted drafts. Use `:diff` to inspect versions before publishing.

```text
:open PAGE_ID
:append to_do Prepare the next release
:new Research notes
:format bold true
:search project
:local architecture
```

### Tasks

```text
:table DATA_SOURCE_ID
:board DATA_SOURCE_ID Status
:task Check the CLI round trip
:view VIEW_ID
```

In a table, `h/l` selects a cell and `e` edits its property. On a board, `h/l`
selects a column, `j/k` selects a card, and `H/L` changes its status.
Structural commands such as `:append`, `:new` and `:task` execute the requested
operation immediately when online. Use `:refresh` to reload afterward.

### Scripts

```sh
zottion page get PAGE_ID --content --all --json
zottion block update BLOCK_ID --file - --json
zottion page create --data-source SOURCE_ID --title 'Review the API' --json
zottion page property set PAGE_ID --property Status --value-json '{"status":{"name":"Done"}}' --json
zottion data-source query SOURCE_ID --filter-file filter.json --all --json
zottion --offline draft list --json
zottion completions bash
```

Results go to stdout; diagnostics go to stderr. Lists expose a cursor; `--all`
follows pagination. `--json` produces schema version 1, including partial-result
metadata. Script commands never initiate browser login. [CLI contract](docs/cli.md).

## Keys

| Keys | Action |
|:---|:---|
| `j/k`, arrows | Select blocks, rows or cards |
| Enter | Open a page or edit a text block |
| `i`, `v` in EdTUI | Insert / Visual mode |
| Esc, then Esc | Return to Normal mode, then block navigation |
| `h/l` | Collapse/load children; select table cells or board columns |
| `Ctrl+W` | Switch navigation/document focus |
| Tab | Toggle properties |
| `Ctrl+P`, `?` | Command palette / help |
| `:w`, `:q`, `:wq` | Publish / exit with drafts / publish and exit |
| `:diff`, `:conflict` | Inspect changes and resolve a conflict explicitly |
| `:drafts`, `:apply ID` | Inspect drafts / explicitly submit one |
| `:operations`, `:check ID` | Inspect the journal / verify an operation |

## Drafts and writes

A draft is persisted before its request is sent. Before writing, zottion reads
the target again and compares the fields it will change. Conflicts keep the base,
local and remote versions. Notion's API does not provide a confirmed CAS mechanism,
so this check cannot eliminate every race with another client.

**An ambiguous write is never retried automatically.** Verify it with
`operation show` / `operation check`. For a create operation, check its parent
in Notion before explicitly recording whether it succeeded. `--yes` confirms an
action; it does not bypass conflict checks.

Offline mode reads loaded content and retains drafts. It does not send an
automatic queue when connectivity returns. Clearing the cache retains drafts and
the journal. Profiles, workspaces and credential identities isolate local state.
OAuth tokens are stored together in Secret Service/Keychain; an unavailable keychain
does not trigger a plaintext fallback. [Recovery details](README.ru.md#сохранность-и-восстановление).

## Documentation

[Русское руководство](README.ru.md) · [CLI](docs/cli.md) ·
[Capabilities](docs/capabilities.md) · [OAuth](docs/oauth.md) ·
[Architecture](ARCHITECTURE.md) · [Verification](docs/verification.md).

The detailed technical documents are currently in Russian.

## Development

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked
python3 scripts/check_terminal.py
cargo build --release --workspace --locked --target x86_64-unknown-linux-gnu
python3 scripts/package.py --target x86_64-unknown-linux-gnu
```

`rust-toolchain.toml` pins Rust 1.91.0. CI checks Linux and macOS. The manual
`packages` workflow builds Linux/macOS x86_64/ARM64 archives and checksums without
publishing a release or deploying the OAuth service.

Screenshots can be reproduced from a release build with `uv run
scripts/capture_screenshots.py --binary PATH_TO_ZOTTION --font PATH_TO_MONOSPACE_FONT`.
The capture tool uses temporary synthetic state and never contacts Notion.

[Contributing](CONTRIBUTING.md) · [Report a bug](https://github.com/IgorKramar/zottion/issues) · [Security](SECURITY.md).

## License

[MIT](LICENSE) · Copyright © 2026 [Igor Kramar](https://github.com/IgorKramar).
