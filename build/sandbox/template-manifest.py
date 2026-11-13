#!/usr/bin/env python3
import datetime
import json
import os
import subprocess
import sys


def command(*args):
    return subprocess.check_output(args, text=True, stderr=subprocess.DEVNULL, timeout=15).strip()


def package(name):
    return command("dpkg-query", "-W", "-f=${Package} ${Version}", name)


os_release = {}
with open("/etc/os-release") as source:
    for line in source:
        key, _, value = line.strip().partition("=")
        os_release[key] = value.strip('"')

print(json.dumps({
    "name": "Ubuntu Desktop",
    "os": os_release["PRETTY_NAME"],
    "desktop": command("gnome-shell", "--version") + " / X11",
    "browser": command("/opt/chromium/chrome", "--version"),
    "tools": ["computer-use-linux " + sys.argv[1],
              "Playwright MCP " + json.load(open('/opt/agentdrive-mcp/node_modules/@playwright/mcp/package.json'))['version'],
              "Node.js " + command('node', '--version'),
              *[package(name) for name in ["gnome-terminal", "nautilus", "gedit", "xdg-utils", "xclip"]]],
    "fonts": [package(name) for name in ["fonts-noto-cjk", "fonts-noto-color-emoji"]],
    "locale": os.environ.get("LANG", "C.UTF-8"),
    "workspace_path": "/workspace",
    "built_at": datetime.datetime.now(datetime.timezone.utc).isoformat(),
}, ensure_ascii=False, indent=2))
