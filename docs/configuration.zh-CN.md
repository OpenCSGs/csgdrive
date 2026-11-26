# 配置与工作区存储

[English](configuration.md) | [简体中文](configuration.zh-CN.md)

## 启动服务

```sh
agentdrived init --directory /srv/agentdrive
agentdrived serve --config /srv/agentdrive/agentdrive.json
```

在浏览器中打开 `http://127.0.0.1:7421/`。Dashboard、CLI 和 MCP 访问同一个 AgentDrive 服务，共用工作区数据库。需要接入 Kubernetes CSI 或其他 S3 客户端时，请在服务环境中设置 `DRIVE_S3_ACCESS_KEY` 和 `DRIVE_S3_SECRET_KEY`，并配置客户端可以访问的内网地址。

```sh
agentdrived serve --config /srv/agentdrive/agentdrive.json \
  --drive-s3-listen 10.0.0.10:19731 --drive-s3-admin-port 19732
```

CSI 驱动使用上述两项凭据请求 AgentDrive。AgentDrive 访问上游对象存储使用工作区配置中的凭据或服务运行环境中的凭据。

## 启动配置与运行时设置

`init` 创建 `agentdrive.json` 和相关目录。文件路径使用绝对路径。每个数据库目录同一时间由一个服务使用。

| 启动字段 | 用途 |
| --- | --- |
| `data_dir` | 保存 SQLite 数据库、本地文件内容、缓存和临时文件。默认目录为 `<directory>/state`。 |
| `listen` | Dashboard 和 MCP 的回环监听地址，默认为 `127.0.0.1:7421`。 |
| `mount_roots` | 允许本地挂载的根目录，默认为 `<directory>/mounts`。 |
| `directory_roots` | 允许导入和导出的根目录，默认为 `<directory>/exchange`。 |
| `principal` | 服务记录客户端操作时使用的身份，默认为 `owner`。 |

启动配置在重启后生效。新建数据库时，服务从启动文件读取缓存容量、自动协作和 HTTP 来源的初始设置。之后的运行时设置保存在 SQLite。Dashboard 的「设置」页面展示当前配置，「创建工作区」用于选择文件存储位置。

- `GET /api/settings` 返回 `startup`、`runtime`、`revision` 和配置文件路径。返回值会隐藏密钥。
- 修改运行时设置时，将读到的完整 `runtime` 对象和当前 `revision` 提交给 `PUT /api/settings`。配置版本已改变会返回 409。保存成功后，新请求和服务重启都会使用更新后的设置。
- `allowed_origins` 接受完整的 HTTP(S) 来源地址。`cache.max_bytes` 控制本地不可变对象的缓存容量，默认 2 GiB。服务每 30 秒检查一次缓存，保留本地需要使用的对象和正在读取的文件。
- `collaboration.enabled` 默认为 true，控制本地可写挂载的自动协作。S3 挂载的写入方式由 S3 管理接口指定。

## 选择工作区存储

在 Dashboard 创建工作区，或向 `POST /api/workspaces` 提交 `request_id`、`name` 和可选的 `storage` 对象。请求中断后使用同一个 `request_id` 重试。省略 `storage` 会选择本地文件存储。工作区创建时确定存储位置。「设置 → 存储设置」展示该位置，同一位置的访问凭据可通过设置 API 更新。

SQLite 保存工作区、分支、文件路径、版本，以及文件和内容标识之间的对应关系。S3 或 OpenCSG 根据内容标识保存实际文件。备份需要同时保全 SQLite 的一致性备份、本地存储目录中尚待发布的文件和仅在本地保存的文件，以及上游的文件内容。数据库目录包含保存过的后端凭据，应限制其文件系统权限。

### S3 与 OSS

先创建目标 bucket。以下 JSON 可直接作为创建工作区接口的请求内容。

```json
{
  "request_id": "create-shared",
  "name": "shared",
  "storage": {
    "backend": "s3",
    "bucket": "drive-content",
    "prefix": "agentdrive/v1",
    "region": "us-east-1",
    "endpoint": "http://127.0.0.1:9000",
    "allow_http": true
  }
}
```

| 存储字段 | 用途 |
| --- | --- |
| `bucket` | 必填，使用已有 bucket 的名称。 |
| `prefix` | 默认为 `agentdrive/v1`。文件内容路径为 `<prefix>/<workspace UUID>/objects/<content ID>`。 |
| `region` | S3 签名区域，默认为 `us-east-1`。 |
| `endpoint` | 不包含 bucket 名称的 S3 API 地址，客户端使用 path-style 请求。未设置时依次读取 `AWS_ENDPOINT_URL_S3`、`AWS_ENDPOINT_URL` 和 `AWS_ENDPOINT`，再使用 AWS 区域地址。 |
| `allow_http` | 默认为 false。在使用 HTTP 的测试环境中设置为 true。 |
| `credentials` | 可选，包含 `access_key_id`、`secret_access_key` 和可选的 `session_token`。省略后使用服务环境或 IAM 角色提供的凭据。 |

服务环境可以设置 `AWS_ACCESS_KEY_ID`、`AWS_SECRET_ACCESS_KEY`，临时凭据还需提供 `AWS_SESSION_TOKEN`。工作区已配置的凭据优先。适配器也支持 Web Identity、容器角色和实例角色。文件上传使用带条件的对象创建，文件大于 8 MiB 时使用分段上传。后端需要支持强一致的 GET/HEAD、条件创建和分段上传。目录名称和工作区分支名称由 AgentDrive 查询 SQLite。

### OpenCSG Codes 与可选 XNet

创建一个包含初始 Git 提交的 Codes 仓库。可以在 Dashboard 填写存储位置，也可以在创建工作区的请求中使用以下 `storage` 对象。

```json
{
  "backend": "opencsg",
  "endpoint": "https://hub.example.org",
  "repository": "team/drive-content",
  "branch": "main",
  "prefix": "agentdrive/v1",
  "token": "<repository-token>",
  "xnet": false
}
```

令牌需要读写仓库文件的权限。`endpoint` 为 HTTP(S) 来源地址。测试环境使用 HTTP 时设置 `allow_http: true`。Git 存储分支默认为 `main`，存储目录默认为 `agentdrive/v1`。每个 AgentDrive 工作区分支的标识和历史均保存在 SQLite。普通文件按最大 4 MiB 分成内容块，逐块验证 SHA-256，仓库保存块内容，数据库保存块清单和排列顺序。

使用 XNet 时，请在 Codes 仓库开启 XNet，并在创建工作区时勾选 XNet。运行 AgentDrive 的机器需要安装传输依赖。

```sh
python3 -m venv /opt/agentdrive-xnet
/opt/agentdrive-xnet/bin/python -m pip install 'hf-xet==1.1.10'
export AGENTDRIVE_XNET_PYTHON=/opt/agentdrive-xnet/bin/python
```

使用该环境启动服务。Codes 的 preupload 接口为每个非空文件选择传输方式。分类为 LFS 的文件通过 XNet 上传，Git 仓库保存标准 LFS 指针。普通文件使用内容块存储。下载时 AgentDrive 校验文件大小和内容标识。

## 多台机器共享一个分支

让 Dashboard、CLI 和 MCP 客户端访问同一个 AgentDrive 服务。CSI 挂载时给每个卷分配不同的 bucket 别名，在服务端将这些别名绑定到同一工作区的同一分支。

两个桌面都需要编辑时，绑定使用 `write_mode: "serialized"`。服务依次提交对象写入。两个桌面编辑同一个文件时，后一次提交成功的内容成为当前内容。在另一个桌面重新打开文件，或刷新 Dashboard 的分支文件列表，即可查看修改。需要指定唯一写入方时，使用 `exclusive` 模式。在交接前刷新待写文件，通过管理接口交接写入权。已经退出写入的挂载需要创建新的会话才能再次取得写入权。

一个独立运行的 AgentDrive 数据库拥有自己的工作区目录，即使两个服务使用同一个 bucket 或 Git 仓库。迁移服务时请同时恢复数据库和文件内容。另见[架构说明](architecture.zh-CN.md)和 [S3 接口说明](../apps/drive-s3/README.md)。
