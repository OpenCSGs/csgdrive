# Sandbox 第三方代码说明

AgentDrive 的 Sandbox worker 包含 TencentCloud/CubeSandbox 的 hypervisor 源码，固定来源为 commit `4411c96ba8658a959de34693f54f4918db2a432f`。代码存放在 `vendor/cube-hypervisor/`，该目录保留 Cloud Hypervisor、Tencent、rust-vmm、virtiofsd 及其他贡献者的原有版权声明。

- 来源记录和完整文件校验值：`vendor/cube-hypervisor/UPSTREAM.json`。
- CubeSandbox 许可证：`vendor/cube-hypervisor/LICENSE-CUBESANDBOX`。
- Hypervisor 许可证：`vendor/cube-hypervisor/LICENSE-APACHE`、`LICENSE-BSD-3-Clause`。
- virtiofsd 许可证：`vendor/cube-hypervisor/virtiofsd/LICENSE-APACHE`、`LICENSE-BSD-3-Clause`。
- 原文件中的单独声明继续适用。

Ubuntu 桌面模板中的部分配置和启动文件来源于同一 commit 的 `examples/ubuntu-desktop/`。具体文件与 AgentDrive 修改记录见 `build/sandbox/SOURCES.md`，许可证见 `build/sandbox/licenses/CubeSandbox-LICENSE`。

分发 Sandbox worker 时，应同时提供本说明、上述适用许可证和原有归属声明。分发 Ubuntu 模板及 Linux 内核时，还需要按照其中各组件的许可证提供相应材料。Ubuntu 软件包自带的版权文件保留在模板的 `/usr/share/doc/` 中。
