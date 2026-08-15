#!/usr/bin/env python3
"""Print a live Sway container id for the Herdr Kitty window.

A cached id is used only when that container still exists and is Kitty.
Otherwise the first Kitty whose process tree contains herdr is used.
"""
from __future__ import annotations

import argparse
import json
import os
import re
import shutil
import subprocess
import sys
from pathlib import Path


def run_host(args: list[str]) -> subprocess.CompletedProcess[str]:
    if shutil.which(args[0]):
        return subprocess.run(args, text=True, capture_output=True, check=False)
    if Path("/run/.containerenv").is_file() and shutil.which("flatpak-spawn"):
        return subprocess.run(
            ["flatpak-spawn", "--host", *args],
            text=True,
            capture_output=True,
            check=False,
        )
    return subprocess.run(args, text=True, capture_output=True, check=False)


def walk(node: object):
    if not isinstance(node, dict):
        return
    yield node
    for key in ("nodes", "floating_nodes"):
        for child in node.get(key) or []:
            yield from walk(child)


def load_tree() -> dict | None:
    forced = os.environ.get("HERDR_FOCUS_NOTIFY_SWAY_TREE")
    if forced:
        path = Path(forced)
        raw = path.read_text(encoding="utf-8") if path.is_file() else forced
        try:
            value = json.loads(raw)
        except json.JSONDecodeError:
            return None
        return value if isinstance(value, dict) else None
    proc = run_host(["swaymsg", "-t", "get_tree"])
    if proc.returncode != 0 or not proc.stdout:
        return None
    try:
        value = json.loads(proc.stdout)
    except json.JSONDecodeError:
        return None
    return value if isinstance(value, dict) else None


def node_by_id(tree: dict, con_id: str) -> dict | None:
    for node in walk(tree):
        if str(node.get("id") or "") == con_id:
            return node
    return None


def kitty_windows(tree: dict) -> list[dict]:
    out: list[dict] = []
    for node in walk(tree):
        if (node.get("app_id") or "").lower() == "kitty":
            out.append(node)
    return out


def proc_root() -> Path:
    return Path(os.environ.get("HERDR_FOCUS_NOTIFY_PROC_ROOT") or "/proc")


def comm(pid: int) -> str:
    try:
        return (proc_root() / str(pid) / "comm").read_text(encoding="utf-8").strip()
    except OSError:
        return ""


def cmdline(pid: int) -> str:
    try:
        raw = (proc_root() / str(pid) / "cmdline").read_bytes()
    except OSError:
        return ""
    return raw.replace(b"\0", b" ").decode("utf-8", "replace")


def children(pid: int) -> list[int]:
    out: list[int] = []
    task = proc_root() / str(pid) / "task"
    try:
        tids = list(task.iterdir())
    except OSError:
        return out
    for tid in tids:
        try:
            text = (tid / "children").read_text(encoding="utf-8")
        except OSError:
            continue
        out.extend(int(part) for part in text.split() if part.isdigit())
    return out


def looks_like_herdr(pid: int) -> bool:
    name = comm(pid).lower()
    if name == "herdr" or name.startswith("herdr"):
        return True
    return re.search(r"(^|[/\s])herdr(\s|$)", cmdline(pid)) is not None


def pid_has_herdr(pid: int, depth: int = 3) -> bool:
    seen: set[int] = set()
    queue: list[tuple[int, int]] = [(child, 1) for child in children(pid)]
    while queue:
        current, level = queue.pop()
        if current in seen or level > depth:
            continue
        seen.add(current)
        if looks_like_herdr(current):
            return True
        if level < depth:
            queue.extend((child, level + 1) for child in children(current))
    return False


def resolve(tree: dict, cached: str) -> str | None:
    if cached:
        node = node_by_id(tree, cached)
        if node is not None and (node.get("app_id") or "").lower() == "kitty":
            return str(node.get("id"))
    herdr_kitties = []
    for node in kitty_windows(tree):
        try:
            pid = int(node.get("pid") or 0)
        except (TypeError, ValueError):
            pid = 0
        if pid and pid_has_herdr(pid):
            herdr_kitties.append(node)
    if herdr_kitties:
        return str(herdr_kitties[0].get("id"))
    kitties = kitty_windows(tree)
    if len(kitties) == 1:
        return str(kitties[0].get("id"))
    return None


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--cached", default="", help="Cached Sway container id")
    args = parser.parse_args(argv)
    tree = load_tree()
    if tree is None:
        return 1
    con = resolve(tree, str(args.cached or "").strip())
    if not con:
        return 1
    print(con)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
