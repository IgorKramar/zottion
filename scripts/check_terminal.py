#!/usr/bin/env python3
"""Сквозная PTY-проверка с синтетическим кэшем, без сети и keychain."""
import argparse
import errno
import fcntl
import json
import os
from pathlib import Path
import pty
import select
import signal
import sqlite3
import struct
import subprocess
import tempfile
import termios
import time

parser = argparse.ArgumentParser()
parser.add_argument("--binary", default="target/debug/zottion")
args = parser.parse_args()
binary = str(Path(args.binary).resolve())
page_id = "b55c9c91-384d-452b-81db-d1ef79372b75"
block_id = "c02fc1d3-db8b-45c5-a222-27595b15aea7"
def encoded(value):
    return json.dumps(value, ensure_ascii=False, separators=(",", ":"))

with tempfile.TemporaryDirectory(prefix="zottion-pty-") as directory:
    root = Path(directory)
    env = dict(os.environ, NOTION_TOKEN="synthetic-pty-token", ZOTTION_CONFIG_DIR=str(root / "config"), ZOTTION_STATE_DIR=str(root / "state"), TERM="xterm-256color")
    subprocess.run([binary, "--offline", "--json", "page", "create", "--parent", page_id, "--title", "fixture"], env=env, check=True, stdout=subprocess.PIPE, stderr=subprocess.PIPE)
    connection = sqlite3.connect(root / "state/state.sqlite3")
    scope = connection.execute("SELECT scope FROM drafts").fetchone()[0]
    connection.execute("DELETE FROM drafts")
    page = {"object": "page", "id": page_id, "properties": {"title": {"id": "title", "type": "title", "title": [{"type": "text", "text": {"content": "Тестовая страница"}}]}}}
    blocks = [{"object": "block", "id": block_id, "type": "paragraph", "has_children": False, "parent": {"page_id": page_id}, "paragraph": {"rich_text": [{"type": "text", "text": {"content": "Исходный текст"}}]}}, {"object": "block", "id": "58b15747-ff92-4e6b-94dd-ab080940ef04", "type": "unsupported", "unsupported": {}, "has_children": False}]
    for kind, key, value in [("resource", f"pages/{page_id}", page), ("resource", f"blocks/{block_id}", blocks[0]), ("list", encoded([f"blocks/{page_id}/children", None, []]), {"object": "list", "results": blocks, "has_more": False, "next_cursor": None})]:
        connection.execute("INSERT INTO snapshots(scope,kind,id,raw,complete,updated) VALUES(?,?,?,?,1,?)", (scope, kind, key, encoded(value), int(time.time())))
    connection.execute("INSERT INTO snapshots(scope,kind,id,raw,complete,updated) VALUES(?,?,?,?,1,?)", (scope, "navigation", "roots", encoded([{"id":page_id,"title":"Тестовая страница"}]), int(time.time())))
    connection.commit()

    def launch(cancel):
        master, slave = pty.openpty()
        fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", 24, 80, 0, 0))
        before = termios.tcgetattr(slave)
        child = subprocess.Popen([binary, "--offline"] + (["open", page_id] if cancel else []), env=env, stdin=slave, stdout=slave, stderr=slave, start_new_session=True)
        output = bytearray()
        def drain(seconds):
            end = time.monotonic() + seconds
            while time.monotonic() < end:
                ready, _, _ = select.select([master], [], [], min(0.05, max(0, end - time.monotonic())))
                if ready:
                    try:
                        data = os.read(master, 65536)
                    except OSError as error:
                        if error.errno == errno.EIO:
                            break
                        raise
                    if not data:
                        break
                    output.extend(data)
        try:
            startup_deadline = time.monotonic() + 5
            while child.poll() is None and "Исходный текст" not in output.decode(errors="replace") and time.monotonic() < startup_deadline:
                drain(0.1)
            assert child.poll() is None, output.decode(errors="replace")
            assert "Тестовая страница" in output.decode(errors="replace")
            assert termios.tcgetattr(slave) != before
            if cancel:
                os.kill(child.pid, signal.SIGINT)
            else:
                os.write(master, b"i")
                os.write(master, b"\x1b[200~" + "Язык 😀".encode() + b"\x1b[201~")
                drain(0.2)
                fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", 30, 100, 0, 0))
                os.kill(child.pid, signal.SIGWINCH)
                os.write(master, b"\x13\x11")
            deadline = time.monotonic() + 5
            while child.poll() is None and time.monotonic() < deadline:
                drain(0.1)
            assert child.poll() is not None, "TUI не завершился"
            assert child.returncode == (130 if cancel else 0), output.decode(errors="replace")
            assert termios.tcgetattr(slave) == before, "Режим терминала не восстановлен"
        finally:
            if child.poll() is None:
                child.kill()
                child.wait()
            os.close(master)
            os.close(slave)
    launch(False)
    draft = json.loads(connection.execute("SELECT local FROM drafts WHERE id=?", (f"blocks/{block_id}",)).fetchone()[0])
    assert "Язык 😀" in draft["text"]
    launch(True)
    connection.close()
print("PTY: auto-open, i, Ctrl+S/Q, 80x24, Unicode paste, resize, durable draft and SIGINT restore — OK")
