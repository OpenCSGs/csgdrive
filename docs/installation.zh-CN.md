# 安装与启动

[English](installation.md) | [简体中文](installation.zh-CN.md)

下载仓库并进入目录：

```sh
git clone https://github.com/GatewayJ/agentdrive.git
cd agentdrive
```

## 构建

需要 Rust stable（声明的最低版本为 1.90）、用于编译内置 SQLite 的 C 编译器，以及 GNU Make。构建任务通过 Make 的 jobserver 管理并行度。

```sh
make -j "$(getconf _NPROCESSORS_ONLN)" build
make -j "$(getconf _NPROCESSORS_ONLN)" verify
```

| 平台 | 原生文件系统要求 | 构建方式 |
| --- | --- | --- |
| Linux | GeeseFS、FUSE 3 工具、`/dev/fuse` 和挂载权限 | 使用默认 features |
| macOS | GeeseFS 和 [macFUSE](https://macfuse.github.io/) | `make -j "$(sysctl -n hw.ncpu)" build` |
| Windows | MSVC，以及包含 Developer 功能的 [WinFsp 2.1](https://github.com/winfsp/winfsp/releases/tag/v2.1) | PowerShell：`make -j $env:NUMBER_OF_PROCESSORS build` |

Linux 和 macOS 使用 `make install-geesefs` 安装 GeeseFS，并将 `$HOME/.local/bin` 加入服务进程的 `PATH`。使用 `BINDIR` 可以指定安装目录。安装程序会校验发布文件的 SHA-256，挂载前还需安装对应的 FUSE 驱动。Windows 从注册的安装路径加载 WinFsp，状态目录使用本地 NTFS 卷。

macOS 用户需要在系统设置中批准 macFUSE，并按系统提示重新启动。

## 运行

```sh
AGENTDRIVE_DIRECTORY="$HOME/.local/share/agentdrive"
./target/debug/agentdrived init --directory "$AGENTDRIVE_DIRECTORY"
./target/debug/agentdrived serve --config "$AGENTDRIVE_DIRECTORY/agentdrive.json"
```

在另一个终端执行：

```sh
./target/debug/agentdrive tools
./target/debug/agentdrive call workspace_create --request-id create-demo --json '{"name":"demo"}'
./target/debug/agentdrive call workspace_list
```

`init` 创建配置及状态、挂载与文件交换目录。配置已存在时拒绝初始化。Dashboard 和 MCP 直接连接，无需访问令牌。守护进程默认监听 `127.0.0.1:7421`，MCP endpoint 为 `/mcp`。每个状态目录由一个守护进程使用。

通过 stdio 连接 MCP 客户端时，将可执行程序设置为 `agentdrive`，参数设置为 `["mcp"]`。可以通过 `AGENTDRIVE_ENDPOINT` 指定其他守护进程 endpoint。stdio 进程会将请求转发给正在运行的守护进程。

使用 `agentdrive tools` 查看 JSON 输入 schema。通过 MCP 调用命令工具时，请求格式为 `{ "request_id": "stable-id", "request": { ... } }`；CLI 的 `--request-id` 选项会构造该请求结构。查询工具和 `operation_cancel` 直接接收参数。`--json -` 从标准输入读取 JSON。工具响应没有错误时，CLI 返回退出码 0；工具返回错误时返回 2；传输或输入发生错误时返回 1；命令行参数解析错误也使用 2。可执行示例见 [CLI 使用](cli.zh-CN.md)，完整工具参考见 [MCP 参数与能力](mcp.zh-CN.md)。


运行 `./target/debug/agentdrive dashboard --open` 打开 Dashboard，默认进入档案室。接入已有项目目录前，在 `directory_roots` 和 `mount_roots` 中允许访问对应目录，详见[转换已有目录](integrations.zh-CN.md)。
