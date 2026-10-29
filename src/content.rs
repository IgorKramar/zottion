use crate::{Error, Result};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{fmt, str::FromStr};
use uuid::Uuid;

fn parse_id(input: &str) -> Result<Uuid> {
    if let Ok(id) = Uuid::parse_str(input) {
        return Ok(id);
    }
    let url =
        reqwest::Url::parse(input).map_err(|_| Error::input("Ожидается UUID или URL Notion"))?;
    if url.scheme() != "https"
        || !url.host_str().is_some_and(|host| {
            host == "notion.so"
                || host.ends_with(".notion.so")
                || host == "notion.com"
                || host.ends_with(".notion.com")
                || host == "notion.site"
                || host.ends_with(".notion.site")
        })
    {
        return Err(Error::input("Ожидается HTTPS URL Notion"));
    }
    let last = url
        .path_segments()
        .and_then(|mut parts| parts.next_back())
        .unwrap_or("");
    let suffix = last.get(last.len().saturating_sub(32)..).unwrap_or("");
    Uuid::parse_str(last)
        .or_else(|_| Uuid::parse_str(suffix))
        .map_err(|_| Error::input("URL не содержит UUID ресурса"))
}
macro_rules! id_type {
    ($($name:ident),+) => {$(
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
        #[serde(transparent)]
        pub struct $name(pub Uuid);
        impl FromStr for $name { type Err = Error; fn from_str(value: &str) -> Result<Self> { parse_id(value).map(Self) } }
        impl fmt::Display for $name { fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result { self.0.fmt(f) } }
    )+};
}
id_type!(PageId, BlockId, DatabaseId, DataSourceId, ViewId);

pub const TEXT_BLOCKS: &[&str] = &[
    "paragraph",
    "heading_1",
    "heading_2",
    "heading_3",
    "bulleted_list_item",
    "numbered_list_item",
    "to_do",
    "toggle",
    "quote",
    "callout",
    "code",
];

pub fn block_type(block: &Value) -> &str {
    block["type"].as_str().unwrap_or("unsupported")
}
pub fn rich_text_plain(rich: &Value) -> String {
    rich.as_array()
        .into_iter()
        .flatten()
        .map(|part| {
            part["text"]["content"]
                .as_str()
                .or_else(|| part["plain_text"].as_str())
                .unwrap_or("")
        })
        .collect()
}
pub fn block_text(block: &Value) -> String {
    let kind = block_type(block);
    if kind == "child_page" {
        return block[kind]["title"].as_str().unwrap_or("").into();
    }
    rich_text_plain(&block[kind]["rich_text"])
}
pub fn page_title(page: &Value) -> String {
    if matches!(page["object"].as_str(), Some("database" | "data_source"))
        && page["title"].is_array()
    {
        return rich_text_plain(&page["title"]);
    }
    page["properties"]
        .as_object()
        .into_iter()
        .flat_map(|props| props.values())
        .find(|prop| prop["type"] == "title" || prop.get("title").is_some())
        .map(|prop| rich_text_plain(&prop["title"]))
        .unwrap_or_else(|| rich_text_plain(&page["title"]))
}

/// Keeps untouched rich-text objects byte-for-byte in JSON. A plain edit may
/// change only one text run; crossing formatting/mention boundaries is explicit.
pub fn text_patch(block: &Value, new_text: &str) -> Result<Value> {
    let kind = block_type(block);
    if !TEXT_BLOCKS.contains(&kind) {
        return Err(Error::unsupported(
            "Этот блок защищён от текстового редактирования",
        ));
    }
    let old = block_text(block);
    let rich = block[kind]["rich_text"]
        .as_array()
        .ok_or_else(|| Error::unsupported("Неизвестный rich text"))?;
    if new_text.chars().count() > 2000 && rich.is_empty() {
        return Err(Error::input(
            "Текстовый фрагмент превышает лимит 2000 символов",
        ));
    }
    if old == new_text {
        return Ok(json!({kind: {"rich_text": rich.iter().map(rich_input).collect::<Vec<_>>()}}));
    }
    let old_chars: Vec<char> = old.chars().collect();
    let new_chars: Vec<char> = new_text.chars().collect();
    let prefix = old_chars
        .iter()
        .zip(&new_chars)
        .take_while(|(a, b)| a == b)
        .count();
    let suffix = old_chars[prefix..]
        .iter()
        .rev()
        .zip(new_chars[prefix..].iter().rev())
        .take_while(|(a, b)| a == b)
        .count();
    let end = old_chars.len() - suffix;
    let inserted: String = new_chars[prefix..new_chars.len() - suffix].iter().collect();
    let mut result = rich.clone();
    if rich.is_empty() {
        result.push(json!({"type":"text","text":{"content":new_text}}));
    } else {
        let mut offset = 0;
        let mut target = None;
        for (index, part) in rich.iter().enumerate() {
            let text = rich_text_plain(&json!([part]));
            let len = text.chars().count();
            if prefix >= offset
                && end <= offset + len
                && part["type"].as_str().unwrap_or("text") == "text"
                && part["text"]["content"].is_string()
            {
                target = Some((index, offset, text));
                break;
            }
            offset += len;
        }
        let (index, offset, text) = target.ok_or_else(|| Error::unsupported("Правка пересекает упоминание или границы rich text; измените отдельный фрагмент через JSON"))?;
        let replacement: String = text
            .chars()
            .take(prefix - offset)
            .chain(inserted.chars())
            .chain(text.chars().skip(end - offset))
            .collect();
        if replacement.chars().count() > 2000 {
            return Err(Error::input(
                "Текстовый фрагмент превышает лимит 2000 символов",
            ));
        }
        result[index]["text"]["content"] = json!(replacement);
        // Read-only render fields are not permitted in PATCH rich-text input.
    }
    Ok(json!({kind: {"rich_text": result.iter().map(rich_input).collect::<Vec<_>>()}}))
}

pub fn rich_input(part: &Value) -> Value {
    let mut value = part.clone();
    if let Some(map) = value.as_object_mut() {
        map.remove("plain_text");
        map.remove("href");
    }
    value
}
pub fn new_block(kind: &str, text: &str) -> Result<Value> {
    if kind == "divider" {
        return Ok(json!({"object":"block","type":"divider","divider":{}}));
    }
    if !TEXT_BLOCKS.contains(&kind) {
        return Err(Error::unsupported("Тип блока не поддерживается"));
    }
    if text.chars().count() > 2000 {
        return Err(Error::input(
            "Текстовый фрагмент превышает лимит 2000 символов",
        ));
    }
    let mut payload = json!({"rich_text":[{"type":"text","text":{"content":text}}]});
    match kind {
        "to_do" => payload["checked"] = json!(false),
        "code" => payload["language"] = json!("plain text"),
        "callout" => payload["icon"] = json!({"type":"emoji","emoji":"💡"}),
        _ => {}
    }
    Ok(json!({"object":"block","type":kind,kind:payload}))
}

/// Build a narrow write payload; callers cannot accidentally PATCH a full GET.
pub fn block_patch(block: &Value, patch: Value) -> Result<Value> {
    let kind = block_type(block);
    if !TEXT_BLOCKS.contains(&kind) && kind != "divider" {
        return Err(Error::unsupported("Неизвестный блок защищён от изменений"));
    }
    let object = patch
        .as_object()
        .ok_or_else(|| Error::input("Ожидается объект PATCH"))?;
    for (key, value) in object {
        if key == "in_trash" || key == "archived" {
            if !value.is_boolean() {
                return Err(Error::input("Флаг корзины должен быть boolean"));
            }
        } else if key == kind {
            let fields = value
                .as_object()
                .ok_or_else(|| Error::input("Ожидается объект содержимого блока"))?;
            if fields.keys().any(|field| !match field.as_str() {
                "rich_text" => TEXT_BLOCKS.contains(&kind),
                "color" => kind != "code" && kind != "divider",
                "checked" => kind == "to_do",
                "language" | "caption" => kind == "code",
                "icon" => kind == "callout",
                _ => false,
            }) {
                return Err(Error::input("Поле блока не разрешено для PATCH"));
            }
        } else {
            return Err(Error::input("Поле блока не разрешено для PATCH"));
        }
    }
    Ok(patch)
}

pub fn terminal_text(text: &str) -> String {
    text.chars()
        .filter(|ch| !ch.is_control() || *ch == '\n' || *ch == '\t')
        .collect()
}

/// Range offsets count Unicode scalar values, matching the CLI contract.
pub fn format_patch(
    block: &Value,
    start: usize,
    end: Option<usize>,
    attribute: &str,
    value: Value,
) -> Result<Value> {
    let kind = block_type(block);
    if !TEXT_BLOCKS.contains(&kind) {
        return Err(Error::unsupported("Блок защищён от форматирования"));
    }
    if !matches!(
        attribute,
        "bold" | "italic" | "underline" | "strikethrough" | "code" | "color" | "link"
    ) {
        return Err(Error::input("Неизвестный атрибут rich text"));
    }
    if !matches!(attribute, "color" | "link") && !value.is_boolean() {
        return Err(Error::input("Атрибут оформления должен быть boolean"));
    }
    if attribute == "color"
        && !value.as_str().is_some_and(|v| {
            [
                "default",
                "gray",
                "brown",
                "orange",
                "yellow",
                "green",
                "blue",
                "purple",
                "pink",
                "red",
                "gray_background",
                "brown_background",
                "orange_background",
                "yellow_background",
                "green_background",
                "blue_background",
                "purple_background",
                "pink_background",
                "red_background",
            ]
            .contains(&v)
        })
    {
        return Err(Error::input("Неизвестный цвет Notion"));
    }
    if attribute == "link" && !value.is_null() {
        let url = reqwest::Url::parse(
            value
                .as_str()
                .ok_or_else(|| Error::input("Ссылка должна быть URL или null"))?,
        )
        .map_err(|_| Error::input("Некорректный URL ссылки"))?;
        if !matches!(url.scheme(), "http" | "https" | "mailto") {
            return Err(Error::input("Разрешены http, https, mailto"));
        }
    }
    let total = block_text(block).chars().count();
    let end = end.unwrap_or(total);
    if start >= end || end > total {
        return Err(Error::input("Диапазон форматирования выходит за текст"));
    }
    let rich = block[kind]["rich_text"]
        .as_array()
        .ok_or_else(|| Error::unsupported("Неизвестный rich text"))?;
    let mut output = Vec::new();
    let mut offset = 0;
    for part in rich {
        let text = rich_text_plain(&json!([part]));
        let len = text.chars().count();
        if end <= offset || start >= offset + len {
            output.push(rich_input(part));
            offset += len;
            continue;
        }
        let a = start.saturating_sub(offset);
        let b = (end - offset).min(len);
        if part["type"] != "text" {
            if a != 0 || b != len || attribute == "link" {
                return Err(Error::unsupported(
                    "Упоминание нельзя разделить или заменить ссылкой",
                ));
            }
            let mut item = rich_input(part);
            if !item["annotations"].is_object() {
                item["annotations"] = json!({});
            }
            item["annotations"][attribute] = value.clone();
            output.push(item);
        } else {
            for (lo, hi, selected) in [(0, a, false), (a, b, true), (b, len, false)] {
                if lo == hi {
                    continue;
                }
                let mut item = rich_input(part);
                item["text"]["content"] =
                    json!(text.chars().skip(lo).take(hi - lo).collect::<String>());
                if selected {
                    if attribute == "link" {
                        item["text"]["link"] = if value.is_null() {
                            Value::Null
                        } else {
                            json!({"url":value})
                        };
                    } else {
                        if !item["annotations"].is_object() {
                            item["annotations"] = json!({});
                        }
                        item["annotations"][attribute] = value.clone();
                    }
                }
                output.push(item);
            }
        }
        offset += len;
    }
    if output.len() > 100 {
        return Err(Error::input("Превышен лимит 100 фрагментов rich text"));
    }
    Ok(json!({kind:{"rich_text":output}}))
}
