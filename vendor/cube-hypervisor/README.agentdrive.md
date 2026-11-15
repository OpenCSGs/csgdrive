# AgentDrive 使用的 hypervisor 源码

来源：https://github.com/TencentCloud/CubeSandbox

固定 commit：`4411c96ba8658a959de34693f54f4918db2a432f`（v0.7.3）。导入范围为该版本的完整 `hypervisor/` 目录，并附带仓库根目录许可证 `LICENSE-CUBESANDBOX`。原文件内容和权限保留，文件校验值记录在 `UPSTREAM.json`。

`runtime/sandbox-worker` 通过 Cargo 本地路径依赖使用本目录。保留独立 Cargo workspace、模块名称、KVM/PVM 实现、内置 virtio-fs 和原有构建文件。导入时没有修改上游代码。

许可证以原文件声明及 `LICENSE-APACHE`、`LICENSE-BSD-3-Clause`、`virtiofsd/LICENSE-*` 为准。CubeSandbox 的 Tencent 修改及 Cloud Hypervisor、rust-vmm、virtiofsd 等上游版权声明保留。

更新源码时需要记录新 commit，更新来源文件校验值，审查上游修改，执行 worker 构建以及桌面、MCP、分支文件和内核选择验证。本目录的修改应在此记录，并在被修改文件中注明。

校验命令：`python3 tests/sandbox/check_sources.py`。
