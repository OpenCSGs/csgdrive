# Sandbox 模板来源

基础镜像：`ghcr.io/tencentcloud/cubesandbox-base:2026.16`，Ubuntu 22.04，保留预装的 envd 及 CubeSandbox 启动文件。

桌面文件来自 TencentCloud/CubeSandbox commit `4411c96ba8658a959de34693f54f4918db2a432f` 的 `examples/ubuntu-desktop/`。`xorg.conf`、`Xwrapper.config`、`novnc-index.html` 保留原内容；AgentDrive 修改了 `Dockerfile`、`start-desktop.sh` 和 `90-cube-ubuntu-desktop.gschema.override`，增加浏览器、图片查看器、字体、统一 MCP 和桌面设置。修改记录日期：2026-11-15。许可证见 `CubeSandbox-LICENSE`（源码目录中位于 `licenses/`）。

来源：https://github.com/TencentCloud/CubeSandbox/tree/4411c96ba8658a959de34693f54f4918db2a432f/examples/ubuntu-desktop 。原有版权和归属声明继续保留。Ubuntu 软件包的许可证文件位于 `/usr/share/doc/<package>/copyright`。

AgentDrive 预装程序的版本和校验值见 `Dockerfile`、`chromium.json`、`mcp/package-lock.json` 和 `assistant/Dockerfile`。CSGClaw 使用 v0.10.0 发布包。模板保留程序归档中的许可证文件。

运行入口为 AgentDrive 的 `guest-init.sh`。Linux 内核由部署者提供，`kernels.json` 保存 KVM/PVM 内核文件的 SHA-256。对应源码、构建配置和许可证资料应随部署产物保存。
