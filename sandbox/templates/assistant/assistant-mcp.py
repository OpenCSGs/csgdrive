"""助手连接桌面用户的统一 MCP，继承实际桌面会话环境。"""
import os
import pwd
import importlib.util
spec = importlib.util.spec_from_file_location("helper", "/usr/local/bin/guest-helper.py")
helper = importlib.util.module_from_spec(spec)
spec.loader.exec_module(helper)
desktop_session, DESKTOP_USER = helper.desktop_session, helper.DESKTOP_USER

environment = desktop_session()
user = pwd.getpwnam(DESKTOP_USER)
if os.getuid() == 0:
    os.setgroups([])
    os.setgid(user.pw_gid)
    os.setuid(user.pw_uid)
os.chdir('/workspace')
os.execve('/usr/local/bin/node', ['node', '/opt/agentdrive-mcp/gateway.mjs'], environment)
