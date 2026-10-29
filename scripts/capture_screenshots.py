#!/usr/bin/env -S uv run --script
# /// script
# requires-python = ">=3.11"
# dependencies = ["pillow==13.0.0", "pyte==0.8.2"]
# ///
"""Снять настоящий TUI с синтетическим кэшем: сеть и keychain не используются."""
import argparse
import codecs
import fcntl
import json
import os
from pathlib import Path
import pty
import select
import sqlite3
import struct
import subprocess
import tempfile
import termios
import time
import uuid

from PIL import Image, ImageDraw, ImageFont
import pyte

parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument("--binary", default="target/x86_64-unknown-linux-gnu/release/zottion")
parser.add_argument("--font", default="/usr/share/fonts/TTF/JetBrainsMonoNerdFont-Regular.ttf")
args = parser.parse_args()
binary = str(Path(args.binary).resolve())
out = Path(__file__).resolve().parent.parent / "docs/screenshots"
out.mkdir(parents=True, exist_ok=True)
page_id = "b55c9c91-384d-452b-81db-d1ef79372b75"
source_id = "c02fc1d3-db8b-45c5-a222-27595b15aea7"
columns, rows = 132, 24

def encoded(value):
    return json.dumps(value, ensure_ascii=False, separators=(",", ":"))

def rich(text):
    return [{"type": "text", "text": {"content": text}}]

def page(id, title):
    return {"object": "page", "id": id, "properties": {"Name": {"id": "title", "type": "title", "title": rich(title)}}}

with tempfile.TemporaryDirectory(prefix="zottion-screenshots-") as directory:
    root = Path(directory)
    env = dict(os.environ, NOTION_TOKEN="synthetic-screenshot-token", ZOTTION_CONFIG_DIR=str(root / "config"), ZOTTION_STATE_DIR=str(root / "state"), TERM="xterm-256color")
    env.pop("NO_COLOR", None)
    subprocess.run([binary, "--offline", "--json", "page", "create", "--parent", page_id, "--title", "fixture"], env=env, check=True, stdout=subprocess.PIPE, stderr=subprocess.PIPE)
    db = sqlite3.connect(root / "state/state.sqlite3")
    scope = db.execute("SELECT scope FROM drafts").fetchone()[0]
    db.execute("DELETE FROM drafts")
    def snapshot(kind, id, raw):
        db.execute("INSERT OR REPLACE INTO snapshots(scope,kind,id,raw,complete) VALUES(?,?,?,?,1)", (scope, kind, id, encoded(raw)))
    notes = page(page_id, "Zottion / Рабочая тетрадь")
    snapshot("resource", f"pages/{page_id}", notes)
    entries = [("heading_1", "Меньше вкладок. Больше законченных мыслей."),
               ("paragraph", "Заметки и задачи Notion — там, где уже открыт терминал."),
               ("heading_2", "Сегодня в фокусе"),
               ("to_do", "Описать идею, пока она свежая"),
               ("to_do", "Связать заметку с задачей"),
               ("to_do", "Закончить начатое и отметить результат"),
               ("heading_2", "Привычный рабочий ритм"),
               ("bulleted_list_item", "Enter — открыть блок. i — начать писать. Esc — вернуться."),
               ("bulleted_list_item", "Черновик сохраняется локально. :w отправляет его в Notion."),
               ("bulleted_list_item", "При конфликте сравнить версии и выбрать решение."),
               ("quote", "Одна мысль. Один блок. Одна точечная правка."),
               ("toggle", "Исследования и решения"),
               ("callout", "Ctrl+P — команды · :board — задачи · :diff — изменения")]
    blocks = []
    for index, (kind, text) in enumerate(entries):
        payload = {} if kind == "divider" else {"rich_text": rich(text)}
        if kind == "to_do": payload["checked"] = index == 5
        raw = {"object": "block", "id": str(uuid.UUID(int=100+index)), "type": kind, "has_children": kind == "toggle", "parent": {"page_id": page_id}, kind: payload}
        blocks.append(raw)
        snapshot("resource", f"blocks/{raw['id']}", raw)
    snapshot("list", encoded([f"blocks/{page_id}/children", None, []]), {"object": "list", "results": blocks, "has_more": False, "next_cursor": None})
    nav_pages = []
    for index, title in enumerate(["Идеи", "Публикация", "Планы на неделю"]):
        id = str(uuid.UUID(int=200+index)); nav_pages.append({"id": id, "title": title}); snapshot("resource", f"pages/{id}", page(id, title))
    snapshot("navigation", "roots", [{"id": page_id, "title": "Рабочая тетрадь"}])
    snapshot("navigation", "favorites", nav_pages[:2])
    snapshot("navigation", "recent", [nav_pages[2], {"id": page_id, "title": "Рабочая тетрадь"}])
    options = [{"id": str(uuid.UUID(int=300+i)), "name": name} for i, name in enumerate(["В планах", "В работе", "Готово"])]
    schema = {"object": "data_source", "id": source_id, "title": rich("Zottion / Задачи"), "properties": {
        "Name": {"id": "title", "type": "title", "title": {}},
        "Status": {"id": "status", "type": "status", "status": {"options": options}},
        "Due": {"id": "due", "type": "date", "date": {}}}}
    snapshot("resource", f"data_sources/{source_id}", schema)
    tasks = []
    for index, (title, status) in enumerate([("Публичный репозиторий", 1), ("Горячие клавиши", 2), ("Сквозная проверка API", 0), ("README и примеры", 1), ("Сохранение черновиков", 2), ("Проверка macOS", 0), ("Просмотр изменений", 2)]):
        row = page(str(uuid.UUID(int=400+index)), title)
        row["parent"] = {"type": "data_source_id", "data_source_id": source_id}
        row["properties"]["Status"] = {"id": "status", "type": "status", "status": options[status]}
        row["properties"]["Due"] = {"id": "due", "type": "date", "date": {"start": "2026-10-30"}}
        tasks.append(row)
        snapshot("resource", f"pages/{row['id']}", row)
    snapshot("list", encoded([f"data_sources/{source_id}/query", {}, []]), {"object": "list", "results": tasks, "has_more": False, "next_cursor": None})
    db.commit()
    master, slave = pty.openpty()
    fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", rows, columns, 0, 0))
    child = subprocess.Popen([binary, "--offline", "open", page_id], env=env, stdin=slave, stdout=slave, stderr=slave, start_new_session=True)
    screen = pyte.Screen(columns, rows)
    stream = pyte.Stream(screen)
    decoder = codecs.getincrementaldecoder("utf-8")("replace")
    def drain(seconds=0.2):
        end = time.monotonic() + seconds
        while time.monotonic() < end:
            ready, _, _ = select.select([master], [], [], min(0.05, max(0, end-time.monotonic())))
            if ready:
                try: data = os.read(master, 65536)
                except OSError: break
                if not data: break
                stream.feed(decoder.decode(data))
    def wait_text(text):
        end = time.monotonic()+5
        while text not in "\n".join(screen.display) and child.poll() is None and time.monotonic()<end: drain()
        assert text in "\n".join(screen.display), "\n".join(screen.display)
    colors = {"default": "#cbd5df", "black": "#10151d", "red": "#e78b8b", "green": "#86d9b8", "brown": "#edc98c", "yellow": "#edc98c", "blue": "#8ebcf0", "magenta": "#bfaff0", "cyan": "#8bd5db", "white": "#d9e2ec", "brightblack": "#6e7d90", "brightwhite": "#f1f5f9"}
    font = ImageFont.truetype(args.font, 17)
    cell_width, cell_height = round(font.getlength("M")), 24
    def color(value, background=False):
        if value == "default": return "#10151d" if background else colors[value]
        # Палитра стандартных ANSI-цветов терминала; данные экрана не меняются.
        palette = {"000000": "#10151d", "00cdcd": "#86d9b8", "e5e5e5": "#d9e2ec"}
        return palette.get(value, colors.get(value, "#"+value if len(value)==6 else "#cbd5df"))
    def save(name, title):
        image = Image.new("RGB", (columns*cell_width+64, rows*cell_height+110), "#0b0f16")
        draw = ImageDraw.Draw(image)
        draw.rounded_rectangle((16,16,image.width-16,image.height-16),radius=18,fill="#10151d",outline="#283441",width=2)
        for index, fill in enumerate(["#e78b8b", "#edc98c", "#86d9b8"]): draw.ellipse((34+index*22,34,44+index*22,44),fill=fill)
        draw.text((120,27), title+"  /  DEMO · OFFLINE",font=font,fill="#91a1b4")
        for y in range(rows):
            for x in range(columns):
                cell = screen.buffer[y][x]
                fg,bg = color(cell.fg),color(cell.bg,True)
                if cell.reverse: fg,bg=bg,fg
                left,top = 32+x*cell_width,76+y*cell_height
                draw.rectangle((left,top,left+cell_width-1,top+cell_height-1),fill=bg)
                if cell.data and cell.data!=" ": draw.text((left,top),cell.data,font=font,fill=fg,stroke_width=0)
        image.save(out/name)
    try:
        wait_text("Рабочая тетрадь")
        os.write(master,b":theme dark\r");drain()
        save("notes.png","zottion · notes")
        os.write(master,f":board {source_id} Status\r".encode());wait_text("Доска")
        save("board.png","zottion · board")
        os.write(master,b":q\r");child.wait(timeout=5)
        assert child.returncode==0
    finally:
        if child.poll() is None: child.kill();child.wait()
        os.close(master);os.close(slave);db.close()
print(out)
