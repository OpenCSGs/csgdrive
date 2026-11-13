mod kernel;
mod proxy;
mod shutdown;
mod templates;
use agentdrive_model::*;
use agentdrive_ports::*;
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::json;
use std::{collections::BTreeMap, net::Ipv4Addr, path::PathBuf, process::Stdio, sync::Arc};
use templates::TemplateConfig;
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    process::{Child, Command},
    sync::Mutex,
};

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default)]
    pub templates: Vec<TemplateConfig>,
    pub worker: PathBuf,
    pub kernel: PathBuf,
    pub image: PathBuf,
    pub directory: PathBuf,
    pub mount_root: PathBuf,
    pub novnc_dir: PathBuf,
    #[serde(default)]
    pub mcp_proxy: Option<PathBuf>,
    #[serde(default = "default_max_instances")]
    pub max_instances: usize,
    #[serde(default)]
    pub networks: Vec<SandboxNetwork>,
}
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SandboxNetwork {
    pub tap: String,
    pub guest_ip: Ipv4Addr,
    pub gateway: Ipv4Addr,
    pub dns: Vec<Ipv4Addr>,
}
impl SandboxNetwork {
    fn validate(&self) -> Result<()> {
        let guest = u32::from(self.guest_ip);
        let gateway = u32::from(self.gateway);
        if self.tap.is_empty()
            || self.tap.len() > 15
            || !self
                .tap
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || c == b'-' || c == b'_')
            || guest & !3 != gateway & !3
            || guest == gateway
            || !(1..=2).contains(&(guest & 3))
            || !(1..=2).contains(&(gateway & 3))
            || self.dns.is_empty()
        {
            return Err(Error::invalid(
                "Sandbox network requires a valid TAP, two hosts in one /30 subnet and DNS servers",
            ));
        }
        Ok(())
    }
    fn kernel_args(&self) -> String {
        let dns = self
            .dns
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join(",");
        format!(
            " agentdrive.ip={}/30 agentdrive.gateway={} agentdrive.dns={dns}",
            self.guest_ip, self.gateway
        )
    }
}
fn default_max_instances() -> usize {
    2
}
impl Config {
    fn check_capacity(&self, memory_gib: u32) -> Result<()> {
        let info = std::fs::read_to_string("/proc/meminfo")?;
        let available = info
            .lines()
            .find(|line| line.starts_with("MemAvailable:"))
            .and_then(|line| line.split_whitespace().nth(1))
            .and_then(|value| value.parse::<u64>().ok())
            .unwrap_or(0);
        if available < (u64::from(memory_gib) + 1) * 1024 * 1024 {
            return Err(Error::new(
                ErrorCode::CapacityExceeded,
                format!("Sandbox requires {memory_gib} GiB of memory plus 1 GiB of host reserve"),
            ));
        }
        Ok(())
    }
}
#[derive(Debug)]
struct Instance {
    status: SandboxStatus,
    child: Child,
    network_slot: Option<usize>,
    proxy: Option<proxy::ProxyRelay>,
}
#[derive(Debug)]
pub struct NativeSandbox {
    config: Config,
    service_port: u16,
    instances: Mutex<BTreeMap<String, Instance>>,
    progress: Mutex<BTreeMap<String, SandboxStatus>>,
}
impl NativeSandbox {
    pub fn open(config: Config, service_port: u16) -> Result<Arc<Self>> {
        if service_port == 0 {
            return Err(Error::invalid("助手访问端口必须有效"));
        }
        for path in [&config.worker, &config.image] {
            if !path.is_absolute() || !path.is_file() {
                return Err(Error::invalid(
                    "Sandbox worker and image must be existing absolute paths",
                ));
            }
        }
        if !config.kernel.is_absolute() {
            return Err(Error::invalid("Sandbox 内核必须使用绝对路径"));
        }
        if !config.directory.is_absolute()
            || !config.mount_root.is_absolute()
            || !config.novnc_dir.is_absolute()
            || !config.novnc_dir.is_dir()
            || config.max_instances == 0
        {
            return Err(Error::invalid(
                "Sandbox directories must be absolute, noVNC assets must exist and max_instances must be positive",
            ));
        }
        if let Some(path) = &config.mcp_proxy
            && (!path.is_absolute() || !path.is_file())
        {
            return Err(Error::invalid(
                "Sandbox MCP proxy must be an existing absolute path",
            ));
        }
        let mut template_ids = std::collections::BTreeSet::from([default_sandbox_template()]);
        for template in &config.templates {
            template.validate()?;
            if !template_ids.insert(template.id.clone()) {
                return Err(Error::invalid("模板标识重复"));
            }
        }
        for (index, network) in config.networks.iter().enumerate() {
            network.validate()?;
            if config.networks[..index].iter().any(|other| {
                other.tap == network.tap
                    || (u32::from(other.guest_ip) & !3) == (u32::from(network.guest_ip) & !3)
            }) {
                return Err(Error::invalid(
                    "Sandbox networks must use distinct TAPs and subnets",
                ));
            }
        }
        std::fs::create_dir_all(&config.directory)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&config.directory, std::fs::Permissions::from_mode(0o700))?;
        }
        Ok(Arc::new(Self {
            config,
            service_port,
            instances: Mutex::new(BTreeMap::new()),
            progress: Mutex::new(BTreeMap::new()),
        }))
    }
    async fn step(&self, id: &str, step: &str) {
        if let Some(status) = self.progress.lock().await.get_mut(id) {
            status.state = SandboxState::Starting;
            status.startup_step = Some(step.to_owned());
            status.message = None;
        }
    }
    #[cfg(unix)]
    async fn guest(&self, id: &str, port: u32) -> Result<BufReader<tokio::net::UnixStream>> {
        let mut stream =
            tokio::net::UnixStream::connect(self.config.directory.join(id).join("vsock.sock"))
                .await?;
        stream
            .write_all(format!("CONNECT {port}\n").as_bytes())
            .await?;
        let mut stream = BufReader::new(stream);
        let mut line = String::new();
        tokio::time::timeout(
            std::time::Duration::from_secs(2),
            stream.read_line(&mut line),
        )
        .await
        .map_err(|_| Error::new(ErrorCode::Unavailable, "Guest connection timed out"))??;
        if !line.starts_with("OK ") {
            return Err(Error::new(
                ErrorCode::Unavailable,
                "Guest connection failed",
            ));
        }
        Ok(stream)
    }
}
#[async_trait]
impl SandboxRuntime for NativeSandbox {
    async fn proxy_config(&self, id: &str) -> Result<SandboxProxyConfig> {
        proxy::read_config(&self.proxy_path(id)?).await
    }
    async fn set_proxy_config(&self, id: &str, config: SandboxProxyConfig) -> Result<()> {
        if self.resolve_template(id, None).await?.profile != "ubuntu-csgclaw" {
            return Err(Error::invalid("当前模板不支持代理配置"));
        }
        proxy::validate(&config)?;
        let path = self.proxy_path(id)?;
        tokio::fs::create_dir_all(path.parent().unwrap()).await?;
        let temporary = path.with_extension("tmp");
        tokio::fs::write(&temporary, serde_json::to_vec(&config)?).await?;
        tokio::fs::rename(temporary, path).await?;
        Ok(())
    }

    async fn template(&self) -> Result<Option<SandboxTemplate>> {
        Ok(Some(
            self.configured_templates()[0]
                .describe(&self.config)
                .await?,
        ))
    }
    async fn templates(&self) -> Result<Vec<SandboxTemplate>> {
        let mut result = Vec::new();
        for template in self.configured_templates() {
            result.push(template.describe(&self.config).await?);
        }
        Ok(result)
    }

    async fn mcp_config(&self, id: &str) -> Result<SandboxMcpConfig> {
        let proxy = self.config.mcp_proxy.as_ref().ok_or_else(|| {
            Error::new(
                ErrorCode::Unavailable,
                "尚未配置宿主机 MCP 连接程序，请设置 mcp_proxy",
            )
        })?;
        let branch: BranchId = id
            .parse()
            .map_err(|_| Error::invalid("Invalid Sandbox branch"))?;
        Ok(SandboxMcpConfig {
            mcp_servers: BTreeMap::from([(
                "ubuntu_desktop".into(),
                SandboxMcpServer {
                    command: "python3".into(),
                    args: vec![
                        proxy.to_string_lossy().into_owned(),
                        "--socket".into(),
                        self.config
                            .directory
                            .join(branch.to_string())
                            .join("vsock.sock")
                            .to_string_lossy()
                            .into_owned(),
                    ],
                },
            )]),
        })
    }

    async fn report_progress(&self, status: SandboxStatus) {
        self.progress.lock().await.insert(status.id.clone(), status);
    }

    async fn desktop_asset(&self, path: &str) -> Result<bytes::Bytes> {
        if std::path::Path::new(path)
            .components()
            .any(|part| !matches!(part, std::path::Component::Normal(_)))
        {
            return Err(Error::invalid("Invalid desktop asset path"));
        }
        let root = self.config.novnc_dir.canonicalize()?;
        let file = root.join(path).canonicalize()?;
        if !file.starts_with(&root) {
            return Err(Error::new(
                ErrorCode::PermissionDenied,
                "Desktop asset escapes its directory",
            ));
        }
        Ok(tokio::fs::read(file).await?.into())
    }

    async fn connect(&self, id: &str, port: u32) -> Result<SandboxConnection> {
        Ok(Box::new(self.guest(id, port).await?))
    }
    async fn start(&self, request: SandboxLaunch) -> Result<SandboxStatus> {
        let mut instances = self.instances.lock().await;
        let id = request.id;
        let template = self
            .resolve_template(&id, request.template_id.as_deref())
            .await?;
        if id != request.binding.branch.to_string() {
            return Err(Error::invalid("Sandbox identity does not match branch"));
        }
        if let Some(instance) = instances.get_mut(&id)
            && instance.child.try_wait()?.is_none()
        {
            return Ok(instance.status.clone());
        }
        let mode = kernel::HostMode::detect(std::path::Path::new("/sys/module"))?;
        let kernel = self.boot_kernel(&template, mode)?;
        tracing::info!(sandbox = %id, ?mode, kernel = %kernel.display(), "Sandbox kernel selected");
        let mut active = 0;
        let mut occupied = Vec::new();
        for instance in instances.values_mut() {
            if instance.child.try_wait()?.is_none() {
                active += 1;
                occupied.extend(instance.network_slot);
            }
        }
        if active >= self.config.max_instances {
            return Err(Error::new(
                ErrorCode::CapacityExceeded,
                "Sandbox instance limit reached",
            ));
        }
        let network_slot = if self.config.networks.is_empty() {
            None
        } else {
            Some(
                (0..self.config.networks.len())
                    .find(|slot| !occupied.contains(slot))
                    .ok_or_else(|| {
                        Error::new(
                            ErrorCode::CapacityExceeded,
                            "No Sandbox network interface is available",
                        )
                    })?,
            )
        };
        self.config.check_capacity(template.memory_gib)?;
        let directory = self.config.directory.join(&id);
        tokio::fs::create_dir_all(&directory).await?;
        tokio::fs::write(
            directory.join("template.json"),
            serde_json::to_vec_pretty(&template)?,
        )
        .await?;
        self.step(&id, "prepare_disk").await;
        let disk = directory.join("ubuntu.ext4");
        if !disk.exists() {
            let result = Command::new("cp")
                .args(["--reflink=auto", "--sparse=always", "--"])
                .arg(&template.image)
                .arg(&disk)
                .status()
                .await?;
            if !result.success() {
                return Err(Error::new(ErrorCode::Io, "Unable to prepare Sandbox disk"));
            }
        }
        let vsock = directory.join("vsock.sock");
        if vsock.exists() {
            tokio::fs::remove_file(&vsock).await?;
        }
        let mut cmdline =
            "console=ttyS0 root=/dev/vda rw init=/usr/local/bin/guest-init.sh".to_owned();
        if let Some(slot) = network_slot {
            let network = &self.config.networks[slot];
            let flags = tokio::fs::read_to_string(format!("/sys/class/net/{}/flags", network.tap))
                .await
                .map_err(|_| {
                    Error::new(
                        ErrorCode::Unavailable,
                        "Sandbox TAP is missing; run prepare-network.sh",
                    )
                })?;
            if u32::from_str_radix(flags.trim().trim_start_matches("0x"), 16).unwrap_or(0) & 1 == 0
            {
                return Err(Error::new(
                    ErrorCode::Unavailable,
                    "Sandbox TAP is down; run prepare-network.sh",
                ));
            }
            cmdline.push_str(&network.kernel_args());
        }
        let mut config = json!({
            "cpus":{"boot_vcpus":template.vcpus,"max_vcpus":template.vcpus},"memory":{"size":u64::from(template.memory_gib)*1024*1024*1024,"shared":true},
            "payload":{"kernel":kernel,"cmdline":cmdline},
            "disks":[{"path":disk}],
            "serial":{"mode":"File","file":directory.join("serial.log")},"console":{"mode":"Null"},
            "fs":[{"tag":"workspace","socket":"","backendfs_config":{"shared_dir":request.binding.path,"read_only":false,"writeback":false}}],
            "vsock":{"cid":3,"socket":vsock}
        });
        if let Some(slot) = network_slot {
            config["net"] = json!([{"tap":self.config.networks[slot].tap, "num_queues":2}]);
        }
        if template.profile == "ubuntu-csgclaw" {
            let state = self
                .config
                .directory
                .parent()
                .ok_or_else(|| Error::invalid("实例目录缺少父目录"))?
                .join("assistant-state")
                .join(&id);
            tokio::fs::create_dir_all(&state).await?;
            let proxy_config = self.proxy_config(&id).await?;
            tokio::fs::write(
                state.join("network.json"),
                serde_json::to_vec(&json!({"proxy_enabled":proxy_config.url.is_some()}))?,
            )
            .await?;
            tokio::fs::write(
                state.join("assistant.json"),
                serde_json::to_vec(&json!({"advertise_base_url":format!(
                    "http://csgclaw.{}.{}.localhost:{}",
                    request.binding.workspace, request.binding.branch, self.service_port
                )}))?,
            )
            .await?;
            config["fs"].as_array_mut().unwrap().push(json!({"tag":"assistant-state","socket":"","backendfs_config":{"shared_dir":state,"read_only":false,"writeback":false}}));
        }
        let proxy = if template.profile == "ubuntu-csgclaw" {
            Some(proxy::ProxyRelay::start(&directory, self.proxy_path(&id)?).await?)
        } else {
            None
        };
        let config_path = directory.join("vm.json");
        tokio::fs::write(&config_path, serde_json::to_vec_pretty(&config)?).await?;
        self.step(&id, "start_vm").await;
        tokio::fs::write(directory.join("serial.log"), b"").await?;
        let log = std::fs::File::create(directory.join("worker.log"))?;
        let mut child = Command::new(&self.config.worker)
            .arg(config_path)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(log)
            .kill_on_drop(true)
            .spawn()?;
        self.step(&id, "wait_desktop").await;
        let mut last_readiness_error = String::new();
        let ready = tokio::time::timeout(std::time::Duration::from_secs(120), async {
            loop {
                if let Some(exit) = child.try_wait()? {
                    return Err(Error::new(
                        ErrorCode::Unavailable,
                        format!("Sandbox worker exited: {exit}"),
                    ));
                }
                if tokio::fs::read_to_string(directory.join("serial.log"))
                    .await
                    .unwrap_or_default()
                    .contains("Kernel panic")
                {
                    return Err(Error::new(
                        ErrorCode::Unavailable,
                        "Ubuntu 启动失败，系统启动程序异常退出，请检查实例启动日志",
                    ));
                }
                if let Ok(stream) = self.guest(&id, 7001).await {
                    let mut response = String::new();
                    match tokio::time::timeout(
                        std::time::Duration::from_secs(30),
                        BufReader::new(stream).read_line(&mut response),
                    )
                    .await
                    {
                        Ok(result) => {
                            result?;
                        }
                        Err(_) => continue,
                    }
                    if let Ok(value) = serde_json::from_str::<serde_json::Value>(&response) {
                        if value.get("ready").and_then(|x| x.as_bool()) == Some(true) {
                            return Ok(());
                        }
                        if let Some(error) = value.get("error").and_then(|x| x.as_str()) {
                            last_readiness_error = error.chars().take(1000).collect();
                        }
                    }
                }
                tokio::time::sleep(std::time::Duration::from_secs(1)).await;
            }
        })
        .await
        .map_err(|_| {
            Error::new(
                ErrorCode::Unavailable,
                if last_readiness_error.is_empty() {
                    "桌面启动检查超时".to_string()
                } else {
                    format!("桌面启动检查超时：{last_readiness_error}")
                },
            )
        })?;
        ready?;
        let status = SandboxStatus {
            template_id: template.id.clone(),
            services: template.links(),
            id: id.clone(),
            workspace: request.binding.workspace,
            branch: request.binding.branch,
            state: SandboxState::Ready,
            desktop_url: Some(format!(
                "/api/workspaces/{}/branches/{}/sandbox/desktop/vnc.html?autoconnect=1&resize=scale&path=api/workspaces/{}/branches/{}/sandbox/desktop/websockify",
                request.binding.workspace,
                request.binding.branch,
                request.binding.workspace,
                request.binding.branch
            )),
            message: None,
            startup_step: Some("ready".into()),
        };
        tokio::fs::write(
            directory.join("status.json"),
            serde_json::to_vec_pretty(&status)?,
        )
        .await?;
        self.progress.lock().await.remove(&id);
        instances.insert(
            id,
            Instance {
                status: status.clone(),
                child,
                network_slot,
                proxy,
            },
        );
        Ok(status)
    }
    async fn stop(&self, id: &str) -> Result<()> {
        self.stop_instance(id, false).await
    }
    async fn force_stop(&self, id: &str) -> Result<()> {
        self.stop_instance(id, true).await
    }
    async fn delete(&self, id: &str) -> Result<()> {
        let branch: BranchId = id
            .parse()
            .map_err(|_| Error::invalid("Invalid Sandbox identity"))?;
        if branch.to_string() != id {
            return Err(Error::invalid("Invalid Sandbox identity"));
        }
        let mut instances = self.instances.lock().await;
        if let Some(instance) = instances.get_mut(id)
            && instance.child.try_wait()?.is_none()
        {
            return Err(Error::new(
                ErrorCode::Busy,
                "请先停止 Sandbox，再删除系统磁盘",
            ));
        }
        let mut progress = self.progress.lock().await;
        if progress.get(id).is_some_and(|status| {
            matches!(
                status.state,
                SandboxState::Creating | SandboxState::Starting | SandboxState::Stopping
            )
        }) {
            return Err(Error::new(
                ErrorCode::Busy,
                "Sandbox 正在执行操作，请完成后再删除",
            ));
        }
        let directory = self.config.directory.join(id);
        match tokio::fs::symlink_metadata(&directory).await {
            Ok(metadata) => {
                if !metadata.is_dir() || metadata.file_type().is_symlink() {
                    return Err(Error::invalid(
                        "Sandbox directory must be a regular directory",
                    ));
                }
                let root = self.config.directory.canonicalize()?;
                let target = directory.canonicalize()?;
                if target.parent() != Some(root.as_path()) {
                    return Err(Error::invalid("Sandbox directory escapes its root"));
                }
                let mut protected = vec![
                    self.config.image.clone(),
                    self.config.kernel.clone(),
                    self.config.worker.clone(),
                    self.config.mount_root.clone(),
                    self.config.novnc_dir.clone(),
                ];
                protected.extend(self.config.mcp_proxy.clone());
                for template in self.configured_templates() {
                    protected.extend([
                        template.kernel.with_file_name("vmlinux-bm"),
                        template.kernel.with_file_name("vmlinux-pvm"),
                    ]);
                    protected.extend([template.image, template.kernel]);
                }
                if let Ok(bytes) = tokio::fs::read(directory.join("vm.json")).await {
                    let value: serde_json::Value = serde_json::from_slice(&bytes)?;
                    if let Some(filesystems) = value.get("fs").and_then(|value| value.as_array()) {
                        for filesystem in filesystems {
                            if let Some(path) = filesystem
                                .pointer("/backendfs_config/shared_dir")
                                .and_then(|value| value.as_str())
                            {
                                protected.push(PathBuf::from(path));
                            }
                        }
                    }
                }
                for path in protected {
                    let resolved = if path.exists() {
                        path.canonicalize()?
                    } else {
                        path
                    };
                    if resolved.starts_with(&target) {
                        return Err(Error::invalid(
                            "Sandbox directory contains a template or shared files",
                        ));
                    }
                }
                tokio::fs::remove_dir_all(&directory).await?;
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        instances.remove(id);
        progress.remove(id);
        Ok(())
    }

    async fn status(&self, id: &str) -> Result<SandboxStatus> {
        if let Some(status) = self.progress.lock().await.get(id).cloned() {
            return Ok(status);
        }
        let mut instances = self.instances.lock().await;
        match instances.get_mut(id) {
            Some(instance) => {
                if let Some(exit) = instance.child.try_wait()? {
                    instance.proxy.take();
                    if !matches!(instance.status.state, SandboxState::Stopped) {
                        instance.status.message = Some(format!("Sandbox 进程已退出：{exit}"));
                    }
                    instance.status.state = SandboxState::Stopped;
                    instance.status.desktop_url = None;
                    instance.status.startup_step = None;
                }
                Ok(instance.status.clone())
            }
            None => {
                let path = self.config.directory.join(id).join("status.json");
                let mut status: SandboxStatus =
                    serde_json::from_slice(&tokio::fs::read(path).await?)?;
                status.state = SandboxState::Stopped;
                status.desktop_url = None;
                Ok(status)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn deletion_runtime(root: &std::path::Path) -> NativeSandbox {
        NativeSandbox {
            service_port: 17421,
            config: Config {
                templates: Vec::new(),
                worker: root.join("worker"),
                kernel: root.join("kernel"),
                image: root.join("template.ext4"),
                directory: root.join("instances"),
                mount_root: root.join("mounts"),
                novnc_dir: root.join("novnc"),
                mcp_proxy: None,
                max_instances: 2,
                networks: Vec::new(),
            },
            instances: Mutex::new(BTreeMap::new()),
            progress: Mutex::new(BTreeMap::new()),
        }
    }

    #[tokio::test]
    async fn guest_handshake_preserves_coalesced_ready_and_flush_responses() {
        use tokio::io::AsyncReadExt;
        let root = tempfile::tempdir().unwrap();
        let runtime = deletion_runtime(root.path());
        for (port, payload) in [(7001, "{\"ready\":true}\n"), (7002, "{\"flushed\":true}\n")] {
            let id = BranchId::new().to_string();
            let directory = runtime.config.directory.join(&id);
            std::fs::create_dir_all(&directory).unwrap();
            let listener = tokio::net::UnixListener::bind(directory.join("vsock.sock")).unwrap();
            let server = tokio::spawn(async move {
                let (stream, _) = listener.accept().await.unwrap();
                let mut reader = BufReader::new(stream);
                let mut request = String::new();
                reader.read_line(&mut request).await.unwrap();
                assert_eq!(request, format!("CONNECT {port}\n"));
                reader
                    .get_mut()
                    .write_all(format!("OK {port}\n{payload}").as_bytes())
                    .await
                    .unwrap();
            });
            let mut guest = runtime.guest(&id, port).await.unwrap();
            let mut response = String::new();
            guest.read_to_string(&mut response).await.unwrap();
            server.await.unwrap();
            assert_eq!(response, payload);
        }
    }

    #[tokio::test]
    async fn failed_flush_preserves_worker_until_explicit_force_stop() {
        let root = tempfile::tempdir().unwrap();
        let runtime = deletion_runtime(root.path());
        let branch = BranchId::new();
        let id = branch.to_string();
        std::fs::create_dir_all(runtime.config.directory.join(&id)).unwrap();
        let child = Command::new("sleep")
            .arg("30")
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        runtime.instances.lock().await.insert(
            id.clone(),
            Instance {
                status: SandboxStatus {
                    template_id: default_sandbox_template(),
                    services: Vec::new(),
                    id: id.clone(),
                    workspace: WorkspaceId::new(),
                    branch,
                    state: SandboxState::Ready,
                    desktop_url: Some("/desktop".into()),
                    message: None,
                    startup_step: None,
                },
                child,
                network_slot: None,
                proxy: None,
            },
        );
        assert_eq!(
            runtime.stop(&id).await.unwrap_err().code,
            ErrorCode::Unavailable
        );
        assert!(
            runtime
                .instances
                .lock()
                .await
                .get_mut(&id)
                .unwrap()
                .child
                .try_wait()
                .unwrap()
                .is_none()
        );
        runtime.force_stop(&id).await.unwrap();
        let status = runtime.status(&id).await.unwrap();
        assert!(matches!(status.state, SandboxState::Stopped));
        assert!(status.desktop_url.is_none());
        assert!(status.message.unwrap().contains("强制停止"));
        assert!(
            runtime
                .instances
                .lock()
                .await
                .get_mut(&id)
                .unwrap()
                .child
                .try_wait()
                .unwrap()
                .is_some()
        );
        runtime.force_stop(&id).await.unwrap();
    }

    #[tokio::test]
    async fn template_selection_preserves_existing_version_and_legacy_disks() {
        let root = tempfile::tempdir().unwrap();
        let mut runtime = deletion_runtime(root.path());
        std::fs::write(&runtime.config.kernel, "kernel").unwrap();
        std::fs::write(&runtime.config.image, "desktop").unwrap();
        let image = root.path().join("assistant.ext4");
        std::fs::write(&image, "assistant").unwrap();
        runtime.config.templates.push(TemplateConfig {
            id: "ubuntu-csgclaw".into(),
            version: "v1".into(),
            image,
            kernel: runtime.config.kernel.clone(),
            profile: "ubuntu-csgclaw".into(),
            vcpus: 4,
            memory_gib: 8,
            disk_gib: 16,
        });
        let branch = BranchId::new().to_string();
        let selected = runtime
            .resolve_template(&branch, Some("ubuntu-csgclaw"))
            .await
            .unwrap();
        assert_eq!(selected.links()[0].id, "csgclaw");
        let directory = runtime.config.directory.join(&branch);
        std::fs::create_dir_all(&directory).unwrap();
        std::fs::write(
            directory.join("template.json"),
            serde_json::to_vec(&selected).unwrap(),
        )
        .unwrap();
        runtime.config.templates[0].version = "v2".into();
        assert_eq!(
            runtime
                .resolve_template(&branch, None)
                .await
                .unwrap()
                .version,
            "v1"
        );
        assert_eq!(
            runtime
                .resolve_template(&branch, Some("ubuntu-desktop"))
                .await
                .unwrap_err()
                .code,
            ErrorCode::Busy
        );
        std::fs::remove_file(directory.join("template.json")).unwrap();
        std::fs::write(directory.join("ubuntu.ext4"), "legacy disk").unwrap();
        assert_eq!(
            runtime.resolve_template(&branch, None).await.unwrap().id,
            "ubuntu-desktop"
        );
        assert!(
            runtime
                .resolve_template(&branch, Some("ubuntu-csgclaw"))
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn existing_sandbox_selects_host_kernel_without_changing_template_or_disk() {
        let root = tempfile::tempdir().unwrap();
        let mut runtime = deletion_runtime(root.path());
        std::fs::write(&runtime.config.image, "desktop").unwrap();
        let mut saved = runtime.configured_templates().remove(0);
        saved.version = "saved-version".into();
        saved.kernel = root.path().join("retired/vmlinux-pvm");
        saved.vcpus = 2;
        let branch = BranchId::new().to_string();
        let directory = runtime.config.directory.join(&branch);
        std::fs::create_dir_all(&directory).unwrap();
        let metadata = serde_json::to_vec(&saved).unwrap();
        std::fs::write(directory.join("template.json"), &metadata).unwrap();
        std::fs::write(directory.join("ubuntu.ext4"), "existing files").unwrap();
        let deployed = root.path().join("deployed");
        std::fs::create_dir(&deployed).unwrap();
        runtime.config.kernel = deployed.join("vmlinux");
        for name in ["vmlinux-bm", "vmlinux-pvm"] {
            std::fs::write(deployed.join(name), name).unwrap();
        }
        for (mode, name) in [
            (kernel::HostMode::Pvm, "vmlinux-pvm"),
            (kernel::HostMode::Kvm, "vmlinux-bm"),
        ] {
            let template = runtime.resolve_template(&branch, None).await.unwrap();
            assert_eq!(
                runtime.boot_kernel(&template, mode).unwrap(),
                deployed.join(name)
            );
            assert_eq!(template.version, "saved-version");
            assert_eq!(template.vcpus, 2);
            assert_eq!(template.kernel, saved.kernel);
        }
        assert_eq!(
            std::fs::read(directory.join("template.json")).unwrap(),
            metadata
        );
        assert_eq!(
            std::fs::read(directory.join("ubuntu.ext4")).unwrap(),
            b"existing files"
        );
    }

    #[tokio::test]
    async fn deletion_preserves_shared_files_and_template_and_clears_failed_startup() {
        let root = tempfile::tempdir().unwrap();
        let runtime = deletion_runtime(root.path());
        let branch = BranchId::new();
        let id = branch.to_string();
        let directory = runtime.config.directory.join(&id);
        let shared = runtime.config.mount_root.join(&id);
        std::fs::create_dir_all(&directory).unwrap();
        std::fs::create_dir_all(&shared).unwrap();
        std::fs::write(&runtime.config.image, "template").unwrap();
        std::fs::write(shared.join("keep.txt"), "branch content").unwrap();
        std::fs::write(directory.join("ubuntu.ext4"), "system disk").unwrap();
        std::fs::write(
            directory.join("vm.json"),
            serde_json::json!({"fs":[{"backendfs_config":{"shared_dir":shared}}]}).to_string(),
        )
        .unwrap();
        runtime
            .report_progress(SandboxStatus {
                template_id: default_sandbox_template(),
                services: Vec::new(),
                id: id.clone(),
                workspace: WorkspaceId::new(),
                branch,
                state: SandboxState::Error,
                desktop_url: None,
                message: None,
                startup_step: None,
            })
            .await;
        runtime.delete(&id).await.unwrap();
        assert!(!directory.exists());
        assert_eq!(
            std::fs::read_to_string(shared.join("keep.txt")).unwrap(),
            "branch content"
        );
        assert_eq!(
            std::fs::read_to_string(&runtime.config.image).unwrap(),
            "template"
        );
        assert_eq!(
            runtime.status(&id).await.unwrap_err().code,
            ErrorCode::NotFound
        );
        runtime.delete(&id).await.unwrap();
    }

    #[tokio::test]
    async fn deletion_rejects_active_instances_and_unsafe_paths() {
        let root = tempfile::tempdir().unwrap();
        let mut runtime = deletion_runtime(root.path());
        let branch = BranchId::new();
        let id = branch.to_string();
        let directory = runtime.config.directory.join(&id);
        std::fs::create_dir_all(&directory).unwrap();
        let child = tokio::process::Command::new("sleep")
            .arg("30")
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let status = SandboxStatus {
            template_id: default_sandbox_template(),
            services: Vec::new(),
            id: id.clone(),
            workspace: WorkspaceId::new(),
            branch,
            state: SandboxState::Error,
            desktop_url: None,
            message: None,
            startup_step: None,
        };
        runtime.instances.lock().await.insert(
            id.clone(),
            Instance {
                status: status.clone(),
                child,
                network_slot: None,
                proxy: None,
            },
        );
        assert_eq!(runtime.delete(&id).await.unwrap_err().code, ErrorCode::Busy);
        let mut instance = runtime.instances.lock().await.remove(&id).unwrap();
        instance.child.kill().await.unwrap();
        runtime
            .report_progress(SandboxStatus {
                state: SandboxState::Starting,
                ..status
            })
            .await;
        assert_eq!(runtime.delete(&id).await.unwrap_err().code, ErrorCode::Busy);
        runtime.progress.lock().await.clear();
        assert!(runtime.delete("../template").await.is_err());
        runtime.config.image = directory.join("template.ext4");
        std::fs::write(&runtime.config.image, "protected").unwrap();
        assert!(runtime.delete(&id).await.is_err());
        assert!(runtime.config.image.exists());
        runtime.config.image = root.path().join("template.ext4");
        std::fs::write(
            directory.join("vm.json"),
            serde_json::json!({"fs":[{"backendfs_config":{"shared_dir":directory}}]}).to_string(),
        )
        .unwrap();
        assert!(runtime.delete(&id).await.is_err());
        std::fs::remove_dir_all(&directory).unwrap();
        std::os::unix::fs::symlink(root.path(), &directory).unwrap();
        assert!(runtime.delete(&id).await.is_err());
        assert!(root.path().exists());
    }

    #[test]
    fn network_requires_distinct_hosts_in_the_same_subnet() {
        let mut network = SandboxNetwork {
            tap: "adsb0".into(),
            guest_ip: "172.30.80.2".parse().unwrap(),
            gateway: "172.30.80.1".parse().unwrap(),
            dns: vec!["223.5.5.5".parse().unwrap()],
        };
        assert!(network.validate().is_ok());
        assert!(
            network
                .kernel_args()
                .contains("agentdrive.ip=172.30.80.2/30")
        );
        network.gateway = network.guest_ip;
        assert!(network.validate().is_err());
        network.gateway = "172.30.80.5".parse().unwrap();
        assert!(network.validate().is_err());
        network.gateway = "172.30.80.1".parse().unwrap();
        network.guest_ip = "172.30.80.3".parse().unwrap();
        assert!(network.validate().is_err());
        network.guest_ip = "172.30.80.2".parse().unwrap();
        network.tap = "../eth0".into();
        assert!(network.validate().is_err());
    }

    #[tokio::test]
    async fn startup_status_remains_available_while_instance_creation_is_locked() {
        let runtime = NativeSandbox {
            service_port: 17421,
            config: Config {
                templates: Vec::new(),
                worker: PathBuf::new(),
                kernel: PathBuf::new(),
                image: PathBuf::new(),
                directory: PathBuf::new(),
                mount_root: PathBuf::new(),
                novnc_dir: PathBuf::new(),
                mcp_proxy: None,
                max_instances: 2,
                networks: Vec::new(),
            },
            instances: Mutex::new(BTreeMap::new()),
            progress: Mutex::new(BTreeMap::new()),
        };
        let branch = BranchId::new();
        runtime
            .report_progress(SandboxStatus {
                template_id: default_sandbox_template(),
                services: Vec::new(),
                id: branch.to_string(),
                workspace: WorkspaceId::new(),
                branch,
                state: SandboxState::Starting,
                desktop_url: None,
                message: None,
                startup_step: Some("wait_desktop".into()),
            })
            .await;
        let _busy = runtime.instances.lock().await;
        let status = tokio::time::timeout(
            std::time::Duration::from_millis(100),
            runtime.status(&branch.to_string()),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(status.startup_step.as_deref(), Some("wait_desktop"));
    }
}
