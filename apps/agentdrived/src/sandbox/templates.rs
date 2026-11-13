use super::*;

#[derive(Clone, Debug, Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct TemplateConfig {
    pub id: String,
    pub version: String,
    pub image: PathBuf,
    pub kernel: PathBuf,
    #[serde(default = "default_profile")]
    pub profile: String,
    #[serde(default = "default_cpus")]
    pub vcpus: u32,
    #[serde(default = "default_memory")]
    pub memory_gib: u32,
    #[serde(default = "default_disk")]
    pub disk_gib: u32,
}
fn default_profile() -> String {
    "ubuntu".into()
}
fn default_cpus() -> u32 {
    4
}
fn default_memory() -> u32 {
    8
}
fn default_disk() -> u32 {
    16
}

impl TemplateConfig {
    pub fn validate(&self) -> Result<()> {
        if self.id.is_empty()
            || !self
                .id
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
            || !matches!(self.profile.as_str(), "ubuntu" | "ubuntu-csgclaw")
            || self.version.is_empty()
            || self.vcpus == 0
            || self.memory_gib == 0
            || self.disk_gib == 0
        {
            return Err(Error::invalid("模板标识、运行配置或资源配置无效"));
        }
        if !self.image.is_absolute() || !self.image.is_file() || !self.kernel.is_absolute() {
            return Err(Error::invalid(
                "模板镜像必须为已存在的绝对路径，内核必须使用绝对路径",
            ));
        }
        Ok(())
    }
    pub fn links(&self) -> Vec<SandboxServiceLink> {
        if self.profile != "ubuntu-csgclaw" {
            return Vec::new();
        }
        [("csgclaw", "打开助手")]
            .into_iter()
            .map(|(id, label)| SandboxServiceLink {
                id: id.into(),
                label: label.into(),
            })
            .collect()
    }
    pub async fn describe(&self, config: &Config) -> Result<SandboxTemplate> {
        let manifest = match tokio::fs::read(self.image.with_extension("template.json")).await {
            Ok(bytes) => Some(serde_json::from_slice(&bytes)?),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => return Err(error.into()),
        };
        let mut capabilities = vec!["desktop".into(), "browser".into(), "mcp".into()];
        capabilities.extend(self.links().into_iter().map(|link| link.id));
        Ok(SandboxTemplate {
            id: self.id.clone(),
            version: self.version.clone(),
            capabilities,
            manifest,
            vcpus: self.vcpus,
            memory_gib: self.memory_gib,
            disk_gib: self.disk_gib,
            max_instances: config.max_instances,
            network_slots: config.networks.len(),
        })
    }
}
impl NativeSandbox {
    pub(super) fn boot_kernel(
        &self,
        template: &TemplateConfig,
        mode: super::kernel::HostMode,
    ) -> Result<PathBuf> {
        // 实例保留系统盘和模板版本，启动内核采用当前部署配置。
        let configured = self
            .configured_templates()
            .into_iter()
            .find(|item| item.id == template.id);
        mode.select_kernel(configured.as_ref().unwrap_or(template).kernel.as_path())
    }
    pub(super) fn configured_templates(&self) -> Vec<TemplateConfig> {
        let mut templates = vec![TemplateConfig {
            id: default_sandbox_template(),
            version: "local".into(),
            image: self.config.image.clone(),
            kernel: self.config.kernel.clone(),
            profile: "ubuntu".into(),
            vcpus: 4,
            memory_gib: 8,
            disk_gib: 16,
        }];
        templates.extend(self.config.templates.clone());
        templates
    }
    pub(super) async fn resolve_template(
        &self,
        id: &str,
        requested: Option<&str>,
    ) -> Result<TemplateConfig> {
        let directory = self.config.directory.join(id);
        let saved = directory.join("template.json");
        let template = match tokio::fs::read(&saved).await {
            Ok(bytes) => serde_json::from_slice::<TemplateConfig>(&bytes)?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                let name = if directory.join("ubuntu.ext4").exists() {
                    default_sandbox_template()
                } else {
                    requested.unwrap_or("ubuntu-desktop").into()
                };
                self.configured_templates()
                    .into_iter()
                    .find(|item| item.id == name)
                    .ok_or_else(|| Error::invalid("模板不存在"))?
            }
            Err(error) => return Err(error.into()),
        };
        if requested.is_some_and(|value| value != template.id) {
            return Err(Error::new(
                ErrorCode::Busy,
                "已有 Sandbox 使用其他模板，请删除后重新创建",
            ));
        }
        template.validate()?;
        Ok(template)
    }
}
