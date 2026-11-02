# 配置与远程存储

[English](configuration.md) | [简体中文](configuration.zh-CN.md)

## 守护进程配置

运行 `agentdrived init --directory /absolute/path/to/agentdrive` 创建 `agentdrive.json`、挂载目录与文件交换目录。通过 `agentdrived serve --config /absolute/path/to/agentdrive/agentdrive.json` 启动服务。配置文件每秒检查一次。Dashboard、CLI 和 MCP 直接连接本地服务，无需访问令牌。

生成的配置包含 `data_dir`、`listen`、`mount_roots`、`directory_roots` 和 `collaboration`。

| 字段 | 含义与默认值 |
| --- | --- |
| `data_dir` | 本地状态目录的绝对路径，初始化使用 `<directory>/state`；每个目录由一个 daemon 使用。 |
| `listen` | 本机回环监听地址，初始化使用 `127.0.0.1:7421`。 |
| `mount_roots` | 允许挂载的绝对路径列表，初始化创建 `<directory>/mounts`。 |
| `directory_roots` | 允许导入导出的绝对路径列表，省略时为 `[]`；初始化允许 `<directory>/exchange`。 |
| `principal` | 工作区操作记录使用的本地身份，默认 `owner`；全部客户端共用此身份。 |
| `allowed_origins` | 额外允许的 HTTP Origin，默认 `[]`；CLI 和 stdio 连接省略 Origin。 |
| `collaboration` | 默认 `{ "enabled": true }`，适用于此 daemon 的全部工作区与可写挂载。 |
| `cache` | 本地版本内容缓存容量 `max_bytes`，默认 `2147483648`（2 GiB），必须为正整数；使用远程存储时生效。 |
| `remote` | 通过 `backend` 选择 S3 或 OpenCSG，省略或 `null` 使用本地存储。已有配置未指定 `backend` 时使用 S3。 |

文件系统路径必须为绝对路径。启动前创建允许的根目录。JSON 字符串不展开 `~` 或环境变量；Windows 路径可以使用转义反斜线或正斜线。未知字段会被拒绝，配置文件最大为 1 MiB。配置和状态通过 daemon 用户的文件系统权限管理。

已有配置未指定 `principal` 时，程序保留原来第一项凭据对应的身份。迁移后忽略 `credentials` 和 `administrators`，保存设置时移除这些字段；原有令牌文件不再读取。远程存储继续使用 OpenCSG token 或 S3 凭据。

## 运行时更新与 Dashboard 设置

通过左下角的“全局设置”或 `/settings` 编辑整个 daemon 的配置。没有工作区时也可以使用设置页面。表单提供自动协作、远程连接、本地缓存容量和 HTTP 访问来源配置。保存时更新启动使用的同一个 `agentdrive.json`。

| 配置 | 生效时间 |
| --- | --- |
| `remote` | 历史内容与目标连接准备完成后生效，无需重启 |
| `cache` | 下次缓存检查，每 30 秒执行一次 |
| `allowed_origins` | 下一次 HTTP 请求，包括已有 MCP 会话 |
| `collaboration.enabled` | 下一次文件操作或协作检查；正在释放和恢复的操作继续执行 |
| `data_dir`、`listen`、`mount_roots`、`directory_roots`、`principal`、进程环境 | 重启 daemon 后 |

Origin 使用 `scheme://host[:port]` 格式，不能包含路径、结尾斜线、凭据、通配符或显式默认端口。同源请求与没有 Origin 的请求可以访问。服务继续使用回环监听地址、Host 与 Origin 检查；此配置不启用 CORS 响应头。

外部文件修改无效时，保留当前配置并在设置页面显示错误。修正文件后，下次检查清除错误。编辑配置时应使用原子替换；启动配置修改需要重启。

- `GET /api/settings` 返回当前 `runtime`、只读 `startup`、`config_path`、`revision` 和 `reload_error`。
- `PUT /api/settings` 接受 `{ "revision": "<GET 返回的 revision>", "runtime": { "cache": { ... }, "allowed_origins": [], "remote": null, "collaboration": { "enabled": true } } }`，需要提供 GET 返回的完整运行配置。

响应会隐藏已保存的远程密钥。配置版本过期或文件被外部修改时返回 409。校验失败保留当前配置与文件。页面发现新版本时保留尚未保存的表单内容，并提供重新载入操作。

## 自动协作

`collaboration.enabled: true` 且已经配置远程存储时，读写挂载自动跟随共享分支，在文件修改时申请写权限，在空闲达到指定时间或登记的工作结束后保存并交还权限。挂载点管理页面调整每个工作分支的交还时间与事件设置。只读挂载和仅使用本地存储的工作区保持原有行为。

在设置页面或 `agentdrive.json` 中将 `collaboration.enabled` 改为 `false`，稳定状态的分支保留当前权限，转为手动管理。已经冻结写入的交还和恢复流程继续完成。重新开启后恢复自动协作。挂载点管理页面展示驱动的句柄统计范围，参见[集成说明](integrations.zh-CN.md#自动协作)。

## 本地缓存容量

`cache.max_bytes` 管理此 daemon 全部工作区的不可变对象缓存。后台每 30 秒检查一次，超过设置容量时清理到容量的 75%，按字节向下取整。程序内部的磁盘剩余空间阈值为 64 MiB：低于此值时也会启动清理，直到同时满足容量目标和剩余空间阈值，或者已经没有可清理的内容。

清理只处理已获得远端确认、没有活动读取者且没有受保护引用的对象。当前分支内容、待发布数据、合并候选内容和固定缓存受到保护。受保护内容较多时，实际占用可能超过设置容量。SQLite 元数据、工作文件和临时文件另外占用磁盘空间。本地模式完整保留历史，不执行缓存清理，Dashboard 隐藏缓存容量输入框。

## 运行期间切换存储

更换远端后端或存储命名空间、禁用远端存储之前，需要在设置中关闭自动协作并完成待处理的交接。协作流程仍依赖当前后端的写权限时，后台会拒绝该配置修改。同一后端和命名空间的凭据或 token 更新会直接刷新连接，可以在自动协作开启期间完成。

在设置页面选择本地、S3 或 OpenCSG 模式，保存即可切换。`remote.enabled: false` 保留当前远程配置和凭据，便于再次启用。每次保存一份远程后端配置。直接修改配置文件也使用相同的切换过程。

启用远程后端时检查目标连接和条件写入行为，复制本地已知的历史内容和分支引用，随后保存配置并更换连接。S3 与 OpenCSG 之间的切换复用此流程，并保留 AgentDrive revision ID 和历史关系。切换回本地时补齐已被缓存清理的历史内容，并停止清理本地历史文件。工作目录和挂载继续使用；待同步操作保留到下一次启用远程存储。

切换期间部分操作会等待历史内容传输完成。内容缺失、存储空间不足、目标分支已更新、凭据错误或配置保存失败时，当前连接继续有效。目标可能已经收到部分不可变对象和分支引用，重复提交会继续处理。拥有尚未完成的分支所有权转移时，需要完成该操作后才能切换。配置文件的外部修改失败时保留原有运行配置，页面显示错误，后续检查继续尝试。S3 连接检查会创建并删除命名空间内的 `.connection-check/` 临时对象。OpenCSG 检查使用 `<prefix>/checks/` 下的临时文件，结束后尝试删除；这些操作仍会保存在仓库的 Git 历史中。

`<data_dir>/meta.db` 是本地 SQLite 数据库，并使用 WAL 文件。远程同步上传不可变文件内容、目录和 inode 索引、版本历史、已发布的操作结果、工作区控制记录和分支引用；数据库文件本身不会上传。目标 daemon 通过 `sync` 的 `direction: "pull"` 将已发布记录导入自己的数据库。本机身份、分支选择、挂载、缓存保留设置、尚未完成的工作和配置分别由各台机器管理。存储结构见[架构说明](architecture.zh-CN.md)。

## OpenCSG Codes 字段

创建专用 Codes 仓库，并通过添加 README 等方式初始化存储分支。在 Dashboard 设置中选择 **OpenCSG Codes**，填写域名、`namespace/repository`、分支和访问 token，保存即可。token 需要仓库文件的读写权限。对应的配置如下：

```json
{
  "remote": {
    "backend": "opencsg",
    "enabled": true,
    "endpoint": "https://opencsg-stg.com",
    "repository": "your-namespace/agentdrive-storage",
    "branch": "main",
    "prefix": "agentdrive/v1",
    "token": "your-access-token",
    "allow_http": false
  }
}
```

| `remote` 字段 | 必填要求 / 默认值 | 行为 |
| --- | --- | --- |
| `backend` | 必填，`opencsg` | 选择 Codes HTTP 适配器。 |
| `enabled` | `true` | 设置为 `false` 使用本地模式，并保留此配置。 |
| `endpoint` | 必填 | HTTP(S) 域名，可包含端口。不包含 API 路径、凭据、查询参数或 fragment。HTTP 需要 `allow_http: true`。 |
| `repository` | 必填 | 已有 Codes 仓库，使用 `namespace/name` 格式。 |
| `branch` | `main` | 已存在且初始化完成的存储分支，名称不包含 `/`。 |
| `prefix` | `agentdrive/v1` | 此分支中保存 AgentDrive 数据的目录。 |
| `token` | 必填 | 仓库访问 token，通过 Bearer 认证发送。 |
| `allow_http` | `false` | 明确启用 HTTP 开发环境地址。 |

仓库名称的各个部分、分支名和目录的各级名称支持 ASCII 字母、数字、`-`、`_` 和 `.`，拒绝首尾句点、连续句点及 `.lock` 后缀。

Dashboard 将 token 保存在配置文件中，Unix 文件权限为 `0600`。GET 设置接口返回空 token。保存时只有域名和仓库均未改变，空 token 才会保留已有值；修改其中任意字段后，需要重新填写 token。HTTP 请求拒绝跳转，错误信息不会包含服务端响应正文。

适配器使用已有的 Codes 文件 HTTP API。读取指定 Git commit 的内容，写入通过 `start_sha` 检查分支当前版本。连接检查验证正常更新、拒绝过期版本更新和重复创建时保留已有文件的行为。服务行为不兼容或检查结果无法确认时，配置不会启用。启动支持离线工作，首次写入执行相同检查。

AgentDrive 继续管理 workspace、branch、autosave、commit、merge、操作结果和写入所有权。Git commit 记录每次存储操作，因此一个 AgentDrive revision 可以产生多个 Git commit。仓库内容使用 AgentDrive 内部格式：每个不可变对象包含 manifest 和最大 4 MiB 的数据块，通过 SHA-256 校验；base64 文本保证二进制内容经过 JSON 文件接口后保持完整。远程引用沿用 S3 后端使用的 AgentDrive 元数据。

存储分支及其内部文件交由 AgentDrive 管理。远程物理删除尚未启用，Git 历史保留存储修改记录，需要为 base64 编码和 Git 历史增长准备容量。每个对象可能产生多次 HTTP 请求和 Git commit，传输时间受仓库大小和服务响应时间影响。AgentDrive 所在机器无需安装 Git 命令。

## S3 字段

以下对象应配置在 `agentdrive.json` 的**顶层 `remote` 字段**中：

```json
{
  "remote": {
    "bucket": "agentdrive",
    "prefix": "agentdrive/v1",
    "region": "us-east-1",
    "endpoint": "http://127.0.0.1:9000",
    "allow_http": true
  }
}
```

这是使用本地 RustFS S3 API endpoint 的配置片段，请保留其他已生成的字段。`endpoint` 是 S3 API 的基础 URL，不包含 bucket 名称、凭据、查询参数或 fragment。适配器使用 path-style 请求，并追加 bucket 名称。端口应采用部署环境提供的 S3 API 端口。

| `remote` 字段 | 必填要求 / 默认值 | 行为 |
| --- | --- | --- |
| `backend` | `s3` | 已有 S3 配置可以继续省略此字段。 |
| `enabled` | `true` | 设置为 `false` 使用本地模式，并保留 S3 配置。 |
| `credentials` | 省略或 `null` | 使用服务环境或 IAM 角色；也可以填写 `access_key_id`、`secret_access_key` 和可选的 `session_token`。 |
| `bucket` | 必填，非空字符串 | 已存在的 bucket 名称。守护进程不会创建 bucket。 |
| `prefix` | `agentdrive/v1` | bucket 内共享的命名空间；首尾 `/` 会被移除，结果必须是有效且非空的对象路径。 |
| `region` | `us-east-1` | 签名使用的 region；AWS S3 应填写 bucket 所在的 region。 |
| `endpoint` | 省略或 `null` | 可选的 S3 基础 URL。没有 endpoint 覆盖设置时，依赖库会生成 AWS regional endpoint。 |
| `allow_http` | `false` | 明确使用 HTTP 开发环境 endpoint 时设置为 `true`；HTTPS endpoint 保持 `false`。 |

使用 AWS S3 时，替换 bucket 和 region，并省略自定义 endpoint：

```json
{
  "remote": {
    "bucket": "your-existing-bucket",
    "prefix": "agentdrive/v1",
    "region": "eu-west-1",
    "allow_http": false
  }
}
```

## S3 凭据与环境变量优先级

Dashboard 可以编辑并保存 S3 凭据到配置文件，Unix 保存权限为 `0600`。GET 设置接口会清空 `secret_access_key` 和 `session_token`；PUT 中同一个 Access Key ID 的空密钥表示保留已有凭据。修改 Session Token 时同时填写 Secret Access Key；将 `credentials` 设为 `null` 使用环境凭据。配置中的凭据优先于环境凭据。

S3 凭据由**守护进程**读取。请在启动 `agentdrived serve` 的终端或服务环境中设置变量。

```sh
export AWS_ACCESS_KEY_ID='your-access-key-id'
export AWS_SECRET_ACCESS_KEY='your-secret-access-key'
# Temporary credentials also require AWS_SESSION_TOKEN.
./target/debug/agentdrived serve --config "$AGENTDRIVE_DIRECTORY/agentdrive.json"
```

PowerShell 使用 `$env:AWS_ACCESS_KEY_ID = 'your-access-key-id'` 和 `$env:AWS_SECRET_ACCESS_KEY = 'your-secret-access-key'`，随后启动 `agentdrived.exe`。

当前锁定版本的 `object_store` 按以下顺序选择凭据来源：

1. `AWS_ACCESS_KEY_ID` 和 `AWS_SECRET_ACCESS_KEY`，以及可选的 `AWS_SESSION_TOKEN`。两个 key 字段需要同时提供。
2. `AWS_WEB_IDENTITY_TOKEN_FILE` 和 `AWS_ROLE_ARN`，以及可选的 `AWS_ROLE_SESSION_NAME`。
3. 用于容器任务角色的 `AWS_CONTAINER_CREDENTIALS_RELATIVE_URI`。
4. 用于 EKS Pod Identity 的 `AWS_CONTAINER_CREDENTIALS_FULL_URI` 和 `AWS_CONTAINER_AUTHORIZATION_TOKEN_FILE`。
5. 前述来源均未提供时，使用实例元数据凭据。

当前适配器不会读取 `~/.aws/credentials`、`AWS_PROFILE` 或 AWS CLI SSO 会话。请导出有效凭据，或者使用支持的角色凭据来源。静态环境变量凭据在启动时读取，替换后需要重新启动守护进程。

AgentDrive 调用 `AmazonS3Builder::from_env()` 后应用自身配置。配置中的 `bucket`、`region` 和 `allow_http`，包括默认值，都会覆盖对应的 AWS 环境变量。配置中的 `endpoint` 会覆盖 `AWS_ENDPOINT` 和 `AWS_ENDPOINT_URL_S3`。省略 `endpoint` 时沿用环境设置；使用默认 AWS regional endpoint 时，需要移除这两个环境变量。适配器会选择 path-style 请求和基于 ETag 的条件写入。

## S3 存储要求与同步验证

存储服务需要支持 GET/HEAD、按前缀列举、对象写入、分段上传和终止上传，以及强一致读取。AgentDrive 使用 `If-None-Match: *` 创建不可变对象，并以相同条件完成分段上传；更新引用时使用携带之前 ETag 的 `If-Match`。条件写入行为见 [AWS S3 文档](https://docs.aws.amazon.com/AmazonS3/latest/userguide/conditional-writes.html)。大于 8 MiB 的文件使用分段上传，分段大小根据文件大小在 8 至 64 MiB 之间选择。存储凭据权限应限制在选定的 bucket 和命名空间内。

守护进程启动时会构建 S3 客户端，但启动成功不能证明 bucket 或凭据可用。按照 [CLI 示例](cli.zh-CN.md)导入内容后，明确执行推送，并查看原始导入操作的结果记录：

```sh
jq -n --arg workspace "$WORKSPACE" '{workspace:$workspace,direction:"push"}' |
  agentdrive call sync --request-id "$RUN_ID-push" --json -
jq -n --arg operation "$(jq -r '.id' "$AGENTDRIVE_DIRECTORY/demo-import.json")" '{operation:$operation}' |
  agentdrive call operation_get --json -
```

检查原始操作的 `remote_confirmed`。`workspace_status.capabilities.remote_configured` 表示当前是否启用 S3 或 OpenCSG，`pending_jobs` 表示队列长度。保存配置但设置 `remote.enabled: false` 时，返回 `remote_configured: false`。启用远程存储后，守护进程每三秒重试待同步任务。`local_saved: true` 的结果记录仍然可能包含远端错误；请保留 operation ID，并参考[恢复说明](operations.zh-CN.md)。

要检查条件写入、并发更新、20 MiB 分段内容和两个守护进程之间的同步，请使用隔离的 [S3/RustFS 测试](validation.zh-CN.md)。这些测试使用独立的测试 bucket，不适合作为生产环境健康检查。

## 多台机器

各守护进程需要选择相同后端和目标：S3 使用相同的 endpoint、bucket、prefix 和签名 region；OpenCSG 使用相同的域名、仓库、存储分支和 prefix。每台机器分别初始化状态目录并获得自己的 `LocationId`，然后使用源守护进程返回的 workspace ID。目标机器上本地 principal 需要是工作区所有者，或者获得适当授权。各机器通过自己的 daemon 连接远程存储。

在源机器执行推送，然后在目标机器使用相同 workspace ID 调用 `sync`，设置 `direction: "pull"`。拉取会导入历史和远端分支引用。可以使用已保留的版本创建本地分支；需要接管已有可写分支时，完成在线 `ownership_transfer`。详见[操作说明](operations.zh-CN.md)和 [MCP 参考](mcp.zh-CN.md)。

配置定义见[文件结构](../crates/agentdrive-config/src/file.rs)、[远程配置](../crates/agentdrive-config/src/remote.rs)、[运行配置管理](../crates/agentdrive-config/src/store.rs)、[S3 适配器](../crates/agentdrive-s3/src/lib.rs)和 [OpenCSG 适配器](../crates/agentdrive-opencsg/src/lib.rs)，依赖版本见 [Cargo.lock](../Cargo.lock)。
