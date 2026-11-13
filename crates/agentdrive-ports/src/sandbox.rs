use agentdrive_model::{BranchId, MountBinding, Principal, Result, WorkspaceId};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SandboxState {
    Creating,
    Starting,
    Ready,
    Stopping,
    Stopped,
    Error,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SandboxStatus {
    #[serde(default = "default_sandbox_template")]
    pub template_id: String,
    #[serde(default)]
    pub services: Vec<SandboxServiceLink>,
    pub id: String,
    pub workspace: WorkspaceId,
    pub branch: BranchId,
    pub state: SandboxState,
    pub desktop_url: Option<String>,
    pub message: Option<String>,
    #[serde(default)]
    pub startup_step: Option<String>,
}

#[derive(Clone, Debug)]
pub struct SandboxLaunch {
    pub template_id: Option<String>,
    pub id: String,
    pub binding: MountBinding,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SandboxMcpServer {
    pub command: String,
    pub args: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SandboxMcpConfig {
    #[serde(rename = "mcpServers")]
    pub mcp_servers: std::collections::BTreeMap<String, SandboxMcpServer>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SandboxTemplateManifest {
    pub name: String,
    pub os: String,
    pub desktop: String,
    pub browser: String,
    pub tools: Vec<String>,
    pub fonts: Vec<String>,
    pub locale: String,
    pub workspace_path: String,
    pub built_at: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SandboxTemplate {
    pub id: String,
    pub version: String,
    pub capabilities: Vec<String>,
    pub manifest: Option<SandboxTemplateManifest>,
    pub vcpus: u32,
    pub memory_gib: u32,
    pub disk_gib: u32,
    pub max_instances: usize,
    pub network_slots: usize,
}

pub fn default_sandbox_template() -> String {
    "ubuntu-desktop".into()
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SandboxServiceLink {
    pub id: String,
    pub label: String,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SandboxProxyConfig {
    pub url: Option<String>,
}

/// Runtime adapters must export only the supplied binding and retain its identity.
#[async_trait]
pub trait SandboxRuntime: Send + Sync {
    async fn proxy_config(&self, _id: &str) -> Result<SandboxProxyConfig> {
        Ok(SandboxProxyConfig::default())
    }
    async fn set_proxy_config(&self, _id: &str, _config: SandboxProxyConfig) -> Result<()> {
        Err(agentdrive_model::Error::invalid("当前模板不支持代理配置"))
    }
    async fn template(&self) -> Result<Option<SandboxTemplate>> {
        Ok(None)
    }
    async fn templates(&self) -> Result<Vec<SandboxTemplate>> {
        Ok(self.template().await?.into_iter().collect())
    }
    async fn mcp_config(&self, id: &str) -> Result<SandboxMcpConfig>;

    async fn report_progress(&self, _status: SandboxStatus) {}
    async fn desktop_asset(&self, path: &str) -> Result<bytes::Bytes>;
    async fn connect(&self, id: &str, port: u32) -> Result<SandboxConnection>;
    async fn start(&self, request: SandboxLaunch) -> Result<SandboxStatus>;
    async fn stop(&self, id: &str) -> Result<()>;
    async fn force_stop(&self, id: &str) -> Result<()>;
    async fn delete(&self, id: &str) -> Result<()>;
    async fn status(&self, id: &str) -> Result<SandboxStatus>;
}

#[async_trait]
pub trait SandboxApi: Send + Sync {
    async fn proxy_config(
        &self,
        actor: &Principal,
        workspace: WorkspaceId,
        branch: BranchId,
    ) -> Result<SandboxProxyConfig>;
    async fn set_proxy_config(
        &self,
        actor: &Principal,
        workspace: WorkspaceId,
        branch: BranchId,
        config: SandboxProxyConfig,
    ) -> Result<()>;
    async fn delete(
        &self,
        actor: &Principal,
        workspace: WorkspaceId,
        branch: BranchId,
    ) -> Result<()>;
    async fn template(&self, actor: &Principal) -> Result<Option<SandboxTemplate>>;
    async fn templates(&self, actor: &Principal) -> Result<Vec<SandboxTemplate>>;
    async fn service(
        &self,
        actor: &Principal,
        workspace: WorkspaceId,
        branch: BranchId,
        service: &str,
    ) -> Result<SandboxConnection>;
    async fn create_with_template(
        &self,
        actor: &Principal,
        workspace: WorkspaceId,
        branch: BranchId,
        template_id: Option<String>,
    ) -> Result<SandboxStatus>;
    async fn mcp_config(
        &self,
        actor: &Principal,
        workspace: WorkspaceId,
        branch: BranchId,
    ) -> Result<SandboxMcpConfig>;

    async fn desktop_asset(
        &self,
        actor: &Principal,
        workspace: WorkspaceId,
        branch: BranchId,
        path: &str,
    ) -> Result<bytes::Bytes>;
    async fn desktop(
        &self,
        actor: &Principal,
        workspace: WorkspaceId,
        branch: BranchId,
    ) -> Result<SandboxConnection>;
    async fn create(
        &self,
        actor: &Principal,
        workspace: WorkspaceId,
        branch: BranchId,
    ) -> Result<SandboxStatus>;
    async fn status(
        &self,
        actor: &Principal,
        workspace: WorkspaceId,
        branch: BranchId,
    ) -> Result<Option<SandboxStatus>>;
    async fn stop(
        &self,
        actor: &Principal,
        workspace: WorkspaceId,
        branch: BranchId,
    ) -> Result<SandboxStatus>;
    async fn force_stop(
        &self,
        actor: &Principal,
        workspace: WorkspaceId,
        branch: BranchId,
    ) -> Result<SandboxStatus>;
}

pub trait SandboxStream: tokio::io::AsyncRead + tokio::io::AsyncWrite + Send + Unpin {}
impl<T: tokio::io::AsyncRead + tokio::io::AsyncWrite + Send + Unpin> SandboxStream for T {}
pub type SandboxConnection = Box<dyn SandboxStream>;
