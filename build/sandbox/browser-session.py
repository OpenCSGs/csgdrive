#!/usr/bin/env python3
"""让桌面入口和浏览器 MCP 共用 Chromium 用户目录与下载目录。"""
import fcntl
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import time
import urllib.request

BINARY = '/opt/chromium/chrome'
PROFILE = Path.home() / '.config/agentdrive-chromium'
DOWNLOADS = Path('/workspace/Downloads')
ENDPOINT = 'http://127.0.0.1:9222/json/version'


def available():
    try:
        with urllib.request.urlopen(ENDPOINT, timeout=1) as response:
            return bool(json.load(response).get('webSocketDebuggerUrl'))
    except (OSError, ValueError):
        return False


def prepare():
    if not os.path.ismount('/workspace'):
        raise RuntimeError('当前分支 /workspace 尚未挂载')
    DOWNLOADS.mkdir(exist_ok=True)
    previous = Path.home() / '.config/chromium'
    if not PROFILE.exists() and previous.is_dir():
        shutil.copytree(previous, PROFILE, ignore=shutil.ignore_patterns('Singleton*', 'Cache', 'Code Cache', 'GPUCache'))
    default = PROFILE / 'Default'
    default.mkdir(parents=True, exist_ok=True)
    preferences = default / 'Preferences'
    values = json.loads(preferences.read_text()) if preferences.exists() else {}
    values.setdefault('download', {}).update(default_directory=str(DOWNLOADS), prompt_for_download=False, directory_upgrade=True)
    values.setdefault('savefile', {})['default_directory'] = str(DOWNLOADS)
    temporary = preferences.with_suffix('.agentdrive-tmp')
    temporary.write_text(json.dumps(values, ensure_ascii=False))
    temporary.replace(preferences)


def command():
    args = [BINARY, '--no-first-run', '--no-default-browser-check', '--password-store=basic',
            '--disable-dev-shm-usage', '--force-renderer-accessibility', '--ozone-platform=x11',
            '--class=chromium', '--remote-debugging-address=127.0.0.1', '--remote-debugging-port=9222',
            '--user-data-dir=' + str(PROFILE)]
    network = Path('/agent-state/network.json')
    if network.is_file() and json.loads(network.read_text()).get('proxy_enabled'):
        args.append('--proxy-server=http://127.0.0.1:18081')
    return args


def ensure():
    runtime = Path(os.environ.get('XDG_RUNTIME_DIR', '/tmp/runtime-user'))
    runtime.mkdir(mode=0o700, exist_ok=True)
    with (runtime / 'agentdrive-browser.lock').open('w') as lock:
        fcntl.flock(lock, fcntl.LOCK_EX)
        if available():
            DOWNLOADS.mkdir(exist_ok=True)
            return
        prepare()
        with (runtime / 'agentdrive-browser.log').open('ab') as log:
            process = subprocess.Popen(command() + ['about:blank'], stdout=log, stderr=log,
                                       start_new_session=True)
        for _ in range(75):
            if available():
                return
            if process.poll() not in (None, 0):
                raise RuntimeError('Chromium 启动失败，请查看 agentdrive-browser.log')
            time.sleep(0.2)
        raise RuntimeError('Chromium 调试接口尚未就绪')


if __name__ == '__main__':
    os.environ['CHROME_DEVEL_SANDBOX'] = '/opt/chromium/chrome_sandbox'
    if '--version' in sys.argv or '--help' in sys.argv:
        os.execv(BINARY, [BINARY] + sys.argv[1:])
    try:
        ensure()
        if sys.argv[1:] != ['--ensure']:
            os.execv(BINARY, command() + (sys.argv[1:] or ['about:blank']))
    except Exception as error:
        print(str(error), file=sys.stderr)
        sys.exit(1)
