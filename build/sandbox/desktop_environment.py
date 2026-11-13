"""读取当前桌面实例的浏览器、分支目录和网络配置。"""
import json
import os
from pathlib import Path
import shutil
import subprocess


def environment_info():
    browser = shutil.which('chromium')
    opener = shutil.which('xdg-open')
    version = None
    if browser:
        result = subprocess.run([browser, '--version'], capture_output=True, text=True, timeout=5)
        if result.returncode == 0:
            version = result.stdout.strip()
    handlers = {}
    if shutil.which('xdg-mime'):
        for scheme in ('http', 'https'):
            result = subprocess.run(
                ['xdg-mime', 'query', 'default', 'x-scheme-handler/' + scheme],
                capture_output=True, text=True, timeout=3)
            handlers[scheme] = result.stdout.strip() if result.returncode == 0 else ''
    routes = Path('/proc/net/route').read_text().splitlines()[1:]
    default_route = any(len(parts := line.split()) > 3 and parts[1] == '00000000'
                        and int(parts[3], 16) & 1 for line in routes)
    dns = []
    if Path('/etc/resolv.conf').exists():
        dns = [parts[1] for line in Path('/etc/resolv.conf').read_text().splitlines()
               if len(parts := line.split()) >= 2 and parts[0] == 'nameserver']
    fonts = {}
    for name, selector in (('chinese', ':lang=zh'), ('emoji', 'Noto Color Emoji')):
        result = subprocess.run(['fc-list', selector, 'family'], capture_output=True,
                                text=True, timeout=5, check=True)
        fonts[name] = bool(result.stdout.strip())
    return {
        'browser': {'name': 'Chromium', 'command': browser, 'version': version, 'desktop_id': 'chromium.desktop',
                    'open_url_command': opener, 'handlers': handlers},
        'fonts': fonts,
        'workspace': {'path': '/workspace', 'mounted': os.path.ismount('/workspace')},
        'network': {'default_route': default_route, 'dns': dns},
    }


def environment_instructions(info):
    browser = info['browser']
    parts = ['当前工具连接 AgentDrive 的 Ubuntu 桌面，MCP 操作与 VNC 显示同一个桌面。']
    if browser['command']:
        parts.append('已安装 Chromium，启动命令为 chromium <URL>，窗口标识为 chromium。')
    if browser['open_url_command']:
        parts.append('桌面终端可以使用 xdg-open <URL> 打开网页。')
    parts.append('统一 MCP 的 browser_* 工具通过 Playwright 操作同一个 Chromium。网页导航、标签页、按钮和表单优先使用 browser_*，系统窗口与终端使用桌面工具。浏览器下载保存到 /workspace/Downloads。')
    parts.append('在 Chromium 输入中文或表情时，点击输入框后使用 paste_text，并读取输入框确认内容。paste_text 会更新当前桌面的剪贴板。')
    parts.append('操作前调用 get_app_state，使用 list_apps 查看运行中的应用。已安装的浏览器及启动入口见上述环境信息；其他应用可以通过桌面应用菜单查找。通过 list_windows 确认目标窗口后再输入；打开网页后检查页面内容和截图。应用尚未运行时，list_apps 可能没有对应条目，判断安装情况时请结合环境信息和桌面应用菜单。')
    if info['workspace']['mounted']:
        parts.append('当前分支已挂载到 /workspace，桌面的 AgentDrive 文件夹指向这里。需要保存到当前分支的文件请写入 /workspace。')
    else:
        parts.append('/workspace 当前未挂载，请报告挂载问题。')
    parts.append('环境检查结果：' + json.dumps(info, ensure_ascii=False, separators=(',', ':')))
    return '\n'.join(parts)
