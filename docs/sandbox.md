# 分支 Ubuntu Sandbox

分支管理页面提供创建、打开桌面、停止和重新启动功能。每个分支使用独立 Ubuntu 系统磁盘，`/workspace` 通过 virtio-fs 连接 AgentDrive 的可写 FUSE 挂载。实例由 agentdrived 的内部 worker 管理，worker 链接 CubeSandbox hypervisor。

正常停止会等待 guest 同步文件。同步失败时实例继续运行，页面显示原因，并提供「强制停止」；输入分支名称确认后会终止进程，尚未同步的修改可能丢失。进程退出等待有超时限制。worker 异常退出后，后台检查会释放分支保护，允许卸载或切换分支；重新启动期间仍保留分支保护。

CSGClaw 的登录回调使用当前实例的 `csgclaw.<工作区>.<分支>.localhost:<AgentDrive 端口>` 地址。AgentDrive 写入实例的助手启动信息，启动脚本通过 CSGClaw 的 `--endpoint` 参数传入地址；已有实例的旧回调配置会被启动参数覆盖。更换 AgentDrive 端口后重新启动 Sandbox 即可更新。STG 登录沿用相同代理入口。

运行主机需要 Linux x86_64、已启用的 KVM 或 PVM、FUSE、至少 9 GiB 可用内存，以及可用的 Docker 镜像构建环境。每个实例使用 4 vCPU、8 GiB 内存和 16 GiB 按需分配的系统磁盘。默认同时运行两个实例，可以通过 `max_instances` 调整数量。

## 桌面使用

在分支页面点击「Sandbox」按钮，在弹框中点击「启动 Sandbox」。启动完成后点击「打开桌面」。弹框集中展示实例状态、启动与停止操作、启动步骤、MCP 配置及使用说明；配置可以直接复制或下载为 JSON。左侧应用栏提供文件管理器、Terminal 和 Chromium。浏览器命令为 `chromium`；Terminal 的命令为 `gnome-terminal`。也可以在应用菜单中搜索应用名称。

当前分支自动挂载到 Ubuntu 的 `/workspace`。桌面上的 `AgentDrive` 文件夹入口指向该目录。在终端执行 `cd /workspace` 即可操作当前分支文件。`/home/user` 是 Ubuntu 的个人目录，需要进入分支的文件应保存到 `/workspace`。实例始终绑定创建时的分支，在网页选择其他分支不会改变已有实例的挂载。

## Agent 与 MCP

模板安装 `xdg-utils`，桌面启动时为用户配置 Chromium 为 HTTP、HTTPS 和 HTML 的默认应用。在桌面终端执行 `xdg-open https://example.com` 即可打开网页。

Chromium 来自官方 Linux 构建，版本与 SHA-256 记录在 `build/sandbox/chromium.json`，构建模板时校验下载内容。该构建需要随模板更新。程序以桌面用户身份运行，并保留 Chromium 自身的进程隔离。模板包含 `fonts-noto-cjk` 和 `fonts-noto-color-emoji`，支持中文与表情显示。

当前桌面会话通过脚本启动，Chromium 使用 `basic` 凭据存储，以便直接启动浏览器。该模式没有系统密钥环提供的独立凭据加密；实例系统磁盘需要按用户数据保护。

中文和表情输入使用桌面 MCP 的 `paste_text`：通过 `get_app_state` 和 `click` 选中输入框，使用 `list_windows` 返回的 `window_id`，然后传入文本。此工具更新 Sandbox 剪贴板，由 computer-use-linux 确认窗口焦点后发送粘贴快捷键。输入完成后读取输入框核对内容。`Ctrl+T` 可以创建标签页，网页按钮可以通过元素信息或坐标点击。

桌面 MCP 初始化响应，以及 `list_apps`、`get_app_state` 的工具描述，会附带当前实例检查到的浏览器命令、URL 入口、分支挂载和网络配置。客户端读取工具列表即可获得这些信息。`list_apps` 返回运行中的应用；Agent 根据环境信息打开浏览器，其他已安装应用通过桌面应用菜单查找，操作后检查窗口、页面内容和截图。

启动页面显示准备分支目录、准备系统磁盘、启动 Ubuntu、等待桌面四个步骤。就绪检查确认 `/workspace` 挂载、窗口管理器和 VNC 连接；浏览器、字体和工具由模板构建及端到端测试验证。启动失败时，页面显示最近一次检查的原因。

Sandbox 对外提供一个统一 MCP 入口，内部运行 computer-use-linux 和 Playwright MCP。沿用 `ubuntu_desktop` 配置名称，现有客户端无需增加条目。当前提供 20 个桌面工具、25 个浏览器工具以及 `sandbox_tools_status`，总计 46 个工具。

- `browser_*`：网页导航、标签页、页面快照、点击、填写表单和下载。
- 原有桌面工具：截图、键盘鼠标、终端和系统窗口，名称保持不变。
- `sandbox_tools_status`：检查内部服务连接状态和工具数量。

宿主机只需要以下一份 stdio MCP 配置。页面「MCP 配置」可以直接复制本分支的实际配置：

```json
{
  "mcpServers": {
    "ubuntu_desktop": {
      "command": "python3",
      "args": [
        "/absolute/path/csgdrive/build/sandbox/mcp-proxy.py",
        "--socket",
        "/absolute/path/agentdrive/sandboxes/BRANCH_ID/vsock.sock"
      ]
    }
  }
}
```

`mcp-proxy.py` 将标准输入输出连接到实例的 vsock 端口 7000，再由 `gateway.mjs` 分发工具请求。需要在运行 AgentDrive 的同一台主机、同一用户下执行。Sandbox 必须已启动；更新或重新启动实例后，在 Agent 客户端重新连接这一份 MCP 配置即可刷新工具列表。模型账号与 API Key 由 Agent 客户端管理。

Chromium 在 Ubuntu 的 `DISPLAY=:0` 运行，用户目录固定为 `/home/user/.config/agentdrive-chromium`。首次更新时复制已有 Chromium 用户目录，保留原目录。桌面图标、`chromium`、`xdg-open` 和 Playwright MCP 共用这个浏览器。调试接口仅监听 guest 内的 `127.0.0.1:9222`，Agent 通过统一 MCP 入口操作浏览器。MCP 断开会释放工具子进程，保留 Chromium 与已打开标签页。

浏览器默认下载目录为 `/workspace/Downloads`，Playwright 输出目录也设置在此处，下载内容会直接进入当前分支。输出文件不启用自动删除。Playwright 操作时网页会在 VNC 中变化；系统鼠标指针移动由桌面工具控制。多个客户端共享同一桌面，操作时需要协调输入。

可以给 Agent 下达：「打开 Python 官网，新建标签页，点击指定按钮，并将下载文件保存到当前分支。」安装在 Ubuntu 内的 Agent 可以直接启动 `/usr/local/bin/node /opt/agentdrive-mcp/gateway.mjs` 连接相同工具入口。

内部服务在 `build/sandbox/mcp/providers.json` 注册。新增服务时填写 `id`、`command`、`args`，用 `prefix` 区分工具名称，再安装对应程序并构建模板。Agent 的配置保持不变，重新连接后获得新的工具列表。工具名称冲突会明确报错，单个服务断开时其他服务仍可使用。当前统一入口合并 MCP tools，转发原始参数、结构化结果、图片和工具错误，支持分页工具列表、请求取消及连接清理。

## 构建

安装 Rust，以及发行版的 `libseccomp-dev`、`libcap-ng-dev`。构建命令：

```bash
cargo build --release -p agentdrived -p agentdrive
cargo build --release --locked --manifest-path runtime/sandbox-worker/Cargo.toml
```

worker 使用 CubeSandbox v0.7.3 对应 commit，并固定 linux-loader 0.13.0。Ubuntu 模板安装 computer-use-linux 0.7.53，下载文件使用 SHA-256 验证。Playwright MCP 固定为 0.0.85，MCP SDK 固定为 1.32.2；Node.js 构建镜像固定 digest，npm 依赖通过 package-lock.json 和 npm ci 安装。

准备 CubeSandbox v0.7.3 的兼容 `vmlinux`，使用下列命令构建模板。输出目录需要尚未存在：

```bash
build/sandbox/build-image.sh /absolute/path/template /absolute/path/vmlinux
```

桌面构建文件参考 [CubeSandbox ubuntu-desktop](https://github.com/TencentCloud/CubeSandbox/tree/v0.7.3/examples/ubuntu-desktop)。系统磁盘直接启动 guest-init.sh，guest helper 通过 vsock 提供桌面连接、截图检查、文件刷新和 MCP 标准输入输出。

## 启动配置

每次启动 Sandbox 时，AgentDrive 检查宿主机已经加载的内核模块：存在 `kvm_pvm` 时使用 PVM，否则在存在 `kvm` 时使用 KVM。宿主机需要预先启用对应模块；程序不会安装宿主机内核或重新启动电脑。

内核选择使用配置中 `kernel` 所在目录的 `vmlinux-bm`（KVM）或 `vmlinux-pvm`（PVM）。KVM 兼容旧配置中的内核文件；PVM 必须提供 `vmlinux-pvm`，缺少文件时启动会返回明确错误。内核目录应当提供与 worker 兼容的两个版本。构建脚本自动复制输入内核同目录的 `vmlinux-pvm`，也可以使用第三个参数指定 PVM 内核文件。

已有 Sandbox 保留系统盘、模板版本、资源和助手数据。启动时按照模板 ID 查找当前部署配置中的内核位置，再根据宿主机模式选择内核；已保存的 `template.json` 中的内核路径不会限制模式切换。已经移除的模板使用保存路径所在目录查找内核。运行中的 Sandbox 在下次启动时应用变化。实际使用的内核记录在实例 `vm.json` 的 `payload.kernel` 和服务日志中。

创建配置文件，全部路径使用绝对路径：

```json
{
  "worker": "/absolute/path/csgdrive/runtime/sandbox-worker/target/release/agentdrive-sandbox-worker",
  "kernel": "/absolute/path/template/vmlinux",
  "image": "/absolute/path/template/ubuntu.ext4",
  "directory": "/absolute/path/agentdrive/sandboxes",
  "mount_root": "/absolute/path/agentdrive/mounts",
  "novnc_dir": "/absolute/path/template/novnc",
  "mcp_proxy": "/absolute/path/csgdrive/build/sandbox/mcp-proxy.py",
  "max_instances": 2,
  "networks": [
    {"tap": "adsb0", "guest_ip": "172.30.80.2", "gateway": "172.30.80.1", "dns": ["223.5.5.5", "1.1.1.1"]},
    {"tap": "adsb1", "guest_ip": "172.30.80.6", "gateway": "172.30.80.5", "dns": ["223.5.5.5", "1.1.1.1"]}
  ]
}
```

准备外网接口，参数使用运行 agentdrived 的用户 UID：

```bash
sudo bash build/sandbox/prepare-network.sh "$(id -u)"
```

脚本创建 `adsb0` 和 `adsb1`，使用 `172.30.80.0/29` 地址范围。运行前应确认该范围未被其他网络使用。每个运行中的实例占用一个接口，停止后释放接口。接口由普通用户打开，网络参数通过内核命令行传给 Ubuntu，启动时配置地址、默认路由和 DNS。省略 `networks` 时实例保持离线；配置了网络但接口不可用时，页面显示启动错误。

转发规则允许实例访问公网及接收响应，阻止访问宿主机、私有网络和其他实例。主机重新启动后需要重新运行准备脚本，或者将脚本配置为 systemd 启动服务。新建实例使用当前模板；已有系统磁盘需要在停止状态下更新所需程序和配置。

`mount_root` 必须位于 AgentDrive 配置的 `mount_roots` 内。运行用户需要访问 `/dev/kvm` 与 `/dev/fuse`。

```bash
AGENTDRIVE_SANDBOX_CONFIG=/absolute/path/sandbox.json \
  target/release/agentdrived serve --config /absolute/path/agentdrive.json
```

打开分支管理页面，点击所需分支的「Sandbox」，在弹框中点击「启动 Sandbox」。启动步骤依次展示准备当前分支目录、准备 Ubuntu 系统磁盘、启动 Ubuntu 实例、等待桌面启动、检查浏览器、字体和桌面工具。进度来自后台实际状态，启动期间持续更新；失败时保留具体步骤和原因，并提供重试入口。实例经过窗口管理器、桌面连接和真实截图检查后显示「打开桌面」。页面切换分支不会改变实例绑定。浏览器使用宿主机上的 noVNC 静态文件，guest 仅提供桌面 WebSocket 数据。

运行中的 Sandbox 会阻止关联挂载被卸载、重新绑定或通过版本恢复替换。停止操作等待 guest 刷新文件，停止 worker，并调用 AgentDrive 保存流程。停止后系统磁盘继续保留。守护进程重新启动后，已有实例显示停止状态，可继续使用原系统磁盘启动。

## 验证

以下脚本通过 guest 内的 computer-use-linux MCP 打开 gedit，输入验证内容，通过 Ctrl+S 保存，然后从宿主机分支挂载读取文件并比对内容。脚本创建独立文件名，保存截图和 MCP 调用记录：

```bash
python3 tests/sandbox/desktop_probe.py \
  --socket /absolute/path/sandboxes/BRANCH_ID/vsock.sock \
  --mount /absolute/path/agentdrive/mounts/BRANCH_ID \
  --artifacts /absolute/path/verification
```

## 当前支持范围

已接入本地分支创建、桌面访问、文件共享、停止与重新启动。远端分支需要已经取得本机写入权限。实例通过 vsock 连接宿主机，通过独立 TAP 网卡和 NAT 访问外网。Ubuntu 模板包含 Chromium 浏览器。宿主机 MCP 客户端通过 `mcp-proxy.py` 连接内部 vsock 端口 7000。用户接管输入控制和跨主机迁移尚未提供。

实例状态保存在各实例目录，操作执行期间由 agentdrived 管理。当前创建请求等待启动结果返回。Ubuntu 图形应用中尚未保存的编辑内容需要用户保存后再停止实例。

完整浏览器端到端测试会创建独立工作区，通过页面创建分支和实例，验证桌面、MCP 文件保存、分支隔离及停止后重新启动。启用 `networks` 时，还会通过 computer-use-linux 验证首次启动和重新启动后的 HTTPS 网页访问。测试结束后停止测试实例，保留验证文件和记录：

```bash
node tests/sandbox/e2e.cjs
```

默认连接 `http://127.0.0.1:17421`，可以使用 `AGENTDRIVE_E2E_URL` 和 `AGENTDRIVE_SANDBOX_CONFIG` 指定测试环境。

通过 computer-use-linux 操作浏览器地址栏、读取网页内容并保存截图：

```bash
python3 tests/sandbox/network_probe.py \
  --socket /absolute/path/sandboxes/BRANCH_ID/vsock.sock \
  --artifacts /absolute/path/network-verification
```

## 沙盒管理页面

侧边栏「沙盒」打开 `/sandboxes`，显示当前用户可以访问的所有工作区的本机沙盒。支持工作区、状态和名称筛选，提供启动、停止、打开桌面，以及启动步骤与 MCP 配置。点击「创建沙盒」打开弹框，选择工作区和尚未创建沙盒的可写分支，点击「创建并启动」后直接显示启动进度。弹框会提示没有可用分支或运行实例达到上限的情况。

「模板内容」显示当前配置模板的系统、桌面、浏览器、字体、工具版本及资源配置。构建镜像时从已安装的软件生成清单，`build-image.sh` 将清单输出到系统镜像旁边的 `ubuntu.template.json`。运行时按照镜像文件名读取同目录的 `.template.json` 文件；没有清单时，页面明确提示详情尚未提供。已有实例继续使用原系统磁盘，模板清单描述新建实例使用的模板。

`GET /api/sandboxes` 返回实例目录与模板信息，使用现有工作区和分支权限检查。模板清单不包含宿主机文件路径或 MCP 配置。

## 统一 MCP 端到端验证

验证脚本从 AgentDrive API 获取单个 MCP 配置，通过同一连接调用浏览器和桌面工具。检查外网网页、新标签页、页面按钮、VNC 桌面截图、下载文件内容以及 MCP 重连后的标签页。测试页面使用唯一文件名，结束后删除页面文件，下载证明保留在当前分支的 Downloads 目录。

```bash
python3 tests/sandbox/playwright_probe.py \
  --workspace WORKSPACE_ID --branch BRANCH_ID \
  --mount /absolute/path/agentdrive/mounts/BRANCH_ID \
  --artifacts /absolute/path/verification
```

统一入口和通信通道的自动化测试：

```bash
npm ci --prefix build/sandbox/mcp --ignore-scripts
node --test tests/sandbox/mcp-gateway.test.mjs
python3 -m unittest discover -s tests/sandbox -p 'test_*.py'
```

### 删除 Sandbox

沙盒页面的实例卡片和分支页面的 Sandbox 弹框均提供“删除 Sandbox”。运行中的实例需要先停止；已停止或启动失败的实例可以删除。确认弹框展示工作区、分支和删除范围，并要求输入完整分支名称。

删除会清除该实例的 Ubuntu 系统磁盘、安装的软件、浏览器登录状态及实例日志。AgentDrive 分支、版本历史、挂载目录以及 `/workspace` 中的文件（包括 `Downloads`）全部保留。后续启动使用当前模板创建新的系统磁盘。

接口为 `POST /api/workspaces/{workspace}/branches/{branch}/sandbox/delete`，JSON 请求体为 `{"confirm_branch":"分支 UUID"}`。接口检查管理权限和分支归属，拒绝删除正在运行或正在启动的实例，并在删除前保存仍然挂载的分支文件。重复删除同一实例可以安全重试。原有 `DELETE /api/workspaces/{workspace}/branches/{branch}/sandbox` 仍用于停止实例。

### 启动就绪检查

启动页面展示四个步骤：准备分支目录、准备 Ubuntu 系统磁盘、启动 Ubuntu 实例、等待桌面启动。就绪检查只确认 GNOME 会话、窗口管理器、VNC 和 noVNC 服务以及 `/workspace` 挂载。启动期间跳过浏览器、字体和 MCP 软件检查，也不执行截图或预先打开 Chromium。桌面辅助功能在模板中默认启用，Playwright 连接时按需打开共用的 Chromium。软件功能通过模板验证脚本单独测试。


## Ubuntu GNOME 与 CSGClaw

`ubuntu-csgclaw` 模板在现有 GNOME 镜像中预装 CSGClaw v0.10.0 完整发布包，包括 Web UI 与 Agent 执行程序。发布包 SHA-256 在构建脚本中校验，模型凭据不写入模板。需要已经构建的 `agentdrive-ubuntu:local` 基础镜像：

```bash
bash build/sandbox/build-assistant-image.sh /absolute/path/ubuntu-csgclaw /absolute/path/vmlinux
```

在 `AGENTDRIVE_SANDBOX_CONFIG` 文件中添加以下 `templates` 条目，重新启动 AgentDrive：

```json
{
  "templates": [{
    "id": "ubuntu-csgclaw",
    "version": "csgclaw-v0.10.0-gnome-1",
    "profile": "ubuntu-csgclaw",
    "image": "/absolute/path/ubuntu-csgclaw/ubuntu.ext4",
    "kernel": "/absolute/path/ubuntu-csgclaw/vmlinux",
    "vcpus": 4,
    "memory_gib": 8,
    "disk_gib": 16
  }]
}
```

在「模板内容」选择 Ubuntu Desktop + CSGClaw，或在创建沙盒时选择该模板。每个实例记录创建时的模板及版本，重新启动使用原系统磁盘。现有普通 Ubuntu 实例继续使用原模板；可以在新分支创建带助手的沙盒。

「打开助手」进入 CSGClaw 完整 Web UI 的新建任务页面，并选中 `agentdrive`。助手工作目录为 `/workspace`，默认接入一份 `ubuntu_desktop` MCP，包含桌面与浏览器工具。无需打开 VNC，聊天即可操作当前分支文件和 Chromium。首次使用需要在 CSGClaw 的模型设置中配置可用模型；程序会在模型配置完成后自动创建分支助手及聊天身份。模型配置、助手与聊天记录保存在实例目录旁的 `assistant-state/BRANCH_ID/csgclaw` 中，独立于分支内容和系统磁盘。

Web UI 使用 `csgclaw.WORKSPACE_ID.BRANCH_ID.localhost:AGENTDRIVE_PORT` 地址。AgentDrive 校验分支权限和请求来源，通过 vsock 转发 HTTP、聊天流和 WebSocket。每个分支使用独立浏览器来源。当前入口供同一台主机使用，Ubuntu 的助手端口保持监听 `127.0.0.1:18080`。

## 分支代理配置

沙盒详情中的「代理配置」接受 HTTP 代理地址，支持通过 CONNECT 访问 HTTPS。地址由宿主机解析，例如 `http://127.0.0.1:7890` 可以使用宿主机上的代理。代理地址需要不含账号、密码、路径和查询参数；留空表示直接联网。保存后在下次启动沙盒时生效，正在进行的任务不会被自动中断。

AgentDrive 通过实例专用 vsock 端口 18081 转发到配置的代理。Ubuntu 中的代理入口为 `127.0.0.1:18081`；CSGClaw 继承 `HTTP_PROXY`、`HTTPS_PROXY`，Chromium 使用 `--proxy-server`。本地 CSGClaw、MCP 和 CDP 连接保持使用回环地址。代理设置存放在宿主机 `proxy-settings/BRANCH_ID.json`，该目录不共享给 Ubuntu。停止沙盒会终止对应的代理转发任务。

删除沙盒保留 `/workspace` 分支文件、助手状态和代理设置；同一分支重新创建带助手的沙盒后可继续使用。助手状态包含模型凭据及聊天内容，管理这些数据时应与本机账号数据采用相同的访问权限。
