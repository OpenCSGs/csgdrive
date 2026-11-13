use crate::Runtime;
use agentdrive_model::*;
use agentdrive_ports::*;
use async_trait::async_trait;
use std::{path::PathBuf, sync::Arc};
use tokio::sync::Mutex;

pub struct SandboxService {
    runtime: Arc<Runtime>,
    services: Services,
    driver: Arc<dyn SandboxRuntime>,
    mount_root: PathBuf,
    operations: Mutex<()>,
}
impl std::fmt::Debug for SandboxService {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SandboxService").finish_non_exhaustive()
    }
}
impl SandboxService {
    pub async fn reconcile_exited(&self) -> Result<()> {
        let Ok(_operation) = self.operations.try_lock() else {
            return Ok(());
        };
        let branches: Vec<_> = self.runtime.sandbox_pins.lock().iter().copied().collect();
        for branch in branches {
            match self.driver.status(&branch.to_string()).await {
                Ok(status) if matches!(status.state, SandboxState::Stopped) => {
                    self.runtime.sandbox_pins.lock().remove(&branch);
                }
                Err(error) if error.code == ErrorCode::NotFound => {
                    self.runtime.sandbox_pins.lock().remove(&branch);
                }
                Err(error) => return Err(error),
                _ => {}
            }
        }
        Ok(())
    }

    pub fn new(
        runtime: Arc<Runtime>,
        services: Services,
        driver: Arc<dyn SandboxRuntime>,
        mount_root: PathBuf,
    ) -> Arc<Self> {
        Arc::new(Self {
            runtime,
            services,
            driver,
            mount_root,
            operations: Mutex::new(()),
        })
    }
}
#[async_trait]
impl SandboxApi for SandboxService {
    async fn proxy_config(
        &self,
        actor: &Principal,
        workspace: WorkspaceId,
        branch: BranchId,
    ) -> Result<SandboxProxyConfig> {
        self.runtime
            .authorize(actor, workspace, Some(branch), Permission::Write)
            .await?;
        self.driver.proxy_config(&branch.to_string()).await
    }
    async fn set_proxy_config(
        &self,
        actor: &Principal,
        workspace: WorkspaceId,
        branch: BranchId,
        config: SandboxProxyConfig,
    ) -> Result<()> {
        let _operation = self.operations.lock().await;
        self.runtime
            .authorize(actor, workspace, Some(branch), Permission::Manage)
            .await?;
        self.driver
            .set_proxy_config(&branch.to_string(), config)
            .await
    }

    async fn delete(
        &self,
        actor: &Principal,
        workspace: WorkspaceId,
        branch: BranchId,
    ) -> Result<()> {
        let _operation = self.operations.lock().await;
        self.runtime
            .authorize(actor, workspace, Some(branch), Permission::Manage)
            .await?;
        match self.driver.status(&branch.to_string()).await {
            Ok(status) if !matches!(status.state, SandboxState::Stopped | SandboxState::Error) => {
                return Err(Error::new(
                    ErrorCode::Busy,
                    "请先停止 Sandbox，再删除系统磁盘",
                ));
            }
            Err(error) if error.code != ErrorCode::NotFound => return Err(error),
            _ => {}
        }
        if let Some(binding) = self
            .runtime
            .state
            .mounts()
            .await?
            .into_iter()
            .find(|binding| {
                binding.branch == branch
                    && binding.state == MountState::Ready
                    && binding.access == AccessMode::ReadWrite
            })
        {
            let saved = self
                .services
                .mounts
                .save(
                    RequestContext {
                        principal: actor.clone(),
                        request_id: format!("sandbox-delete-{}", OperationId::new()),
                    },
                    SaveMount {
                        message: Some("Save branch before deleting Sandbox".into()),
                        mount: binding.id,
                        expected_binding_generation: binding.generation,
                        durability: Durability::Local,
                        wait_for_remote_ms: None,
                    },
                )
                .await?;
            if let Some(error) = saved.error {
                return Err(error);
            }
            if !saved.local_saved {
                return Err(Error::new(
                    ErrorCode::Busy,
                    "分支文件尚未保存，请稍后重试删除",
                ));
            }
        }
        self.driver.delete(&branch.to_string()).await?;
        self.runtime.sandbox_pins.lock().remove(&branch);
        Ok(())
    }

    async fn template(&self, actor: &Principal) -> Result<Option<SandboxTemplate>> {
        self.services.workspace.list(actor).await?;
        self.driver.template().await
    }

    async fn templates(&self, actor: &Principal) -> Result<Vec<SandboxTemplate>> {
        self.services.workspace.list(actor).await?;
        self.driver.templates().await
    }

    async fn service(
        &self,
        actor: &Principal,
        workspace: WorkspaceId,
        branch: BranchId,
        service: &str,
    ) -> Result<SandboxConnection> {
        self.runtime
            .authorize(actor, workspace, Some(branch), Permission::Write)
            .await?;
        let status = self.driver.status(&branch.to_string()).await?;
        if !matches!(status.state, SandboxState::Ready)
            || !status.services.iter().any(|item| item.id == service)
        {
            return Err(Error::new(ErrorCode::Unavailable, "助手服务尚未就绪"));
        }
        let port = match service {
            "csgclaw" => 7011,
            _ => return Err(Error::invalid("未知的助手服务")),
        };
        self.driver.connect(&branch.to_string(), port).await
    }

    async fn mcp_config(
        &self,
        actor: &Principal,
        workspace: WorkspaceId,
        branch: BranchId,
    ) -> Result<SandboxMcpConfig> {
        self.runtime
            .authorize(actor, workspace, Some(branch), Permission::Write)
            .await?;
        self.driver.mcp_config(&branch.to_string()).await
    }

    async fn desktop_asset(
        &self,
        actor: &Principal,
        workspace: WorkspaceId,
        branch: BranchId,
        path: &str,
    ) -> Result<bytes::Bytes> {
        self.runtime
            .authorize(actor, workspace, Some(branch), Permission::Write)
            .await?;
        self.driver.desktop_asset(path).await
    }

    async fn desktop(
        &self,
        actor: &Principal,
        workspace: WorkspaceId,
        branch: BranchId,
    ) -> Result<SandboxConnection> {
        self.runtime
            .authorize(actor, workspace, Some(branch), Permission::Write)
            .await?;
        self.driver.connect(&branch.to_string(), 6080).await
    }

    async fn create(
        &self,
        actor: &Principal,
        workspace: WorkspaceId,
        branch: BranchId,
    ) -> Result<SandboxStatus> {
        self.create_with_template(actor, workspace, branch, None)
            .await
    }

    async fn create_with_template(
        &self,
        actor: &Principal,
        workspace: WorkspaceId,
        branch: BranchId,
        template_id: Option<String>,
    ) -> Result<SandboxStatus> {
        let _operation = self.operations.lock().await;
        self.runtime
            .authorize(actor, workspace, Some(branch), Permission::Write)
            .await?;
        let result = self
            .create_inner(actor, workspace, branch, template_id)
            .await;
        if let Err(error) = &result
            && let Ok(mut status) = self.driver.status(&branch.to_string()).await
        {
            if !matches!(
                status.state,
                SandboxState::Creating | SandboxState::Starting
            ) {
                return result;
            }
            status.state = SandboxState::Error;
            status.message = Some(error.message.clone());
            self.driver.report_progress(status).await;
        }
        result
    }
    async fn status(
        &self,
        actor: &Principal,
        workspace: WorkspaceId,
        branch: BranchId,
    ) -> Result<Option<SandboxStatus>> {
        self.runtime
            .authorize(actor, workspace, Some(branch), Permission::Read)
            .await?;
        self.reconcile_exited().await?;
        match self.driver.status(&branch.to_string()).await {
            Ok(status) => Ok(Some(status)),
            Err(error) if error.code == ErrorCode::NotFound => Ok(None),
            Err(error) => Err(error),
        }
    }
    async fn stop(
        &self,
        actor: &Principal,
        workspace: WorkspaceId,
        branch: BranchId,
    ) -> Result<SandboxStatus> {
        self.stop_inner(actor, workspace, branch, false).await
    }
    async fn force_stop(
        &self,
        actor: &Principal,
        workspace: WorkspaceId,
        branch: BranchId,
    ) -> Result<SandboxStatus> {
        self.stop_inner(actor, workspace, branch, true).await
    }
}

impl SandboxService {
    async fn stop_inner(
        &self,
        actor: &Principal,
        workspace: WorkspaceId,
        branch: BranchId,
        force: bool,
    ) -> Result<SandboxStatus> {
        let _operation = self.operations.lock().await;
        self.runtime
            .authorize(actor, workspace, Some(branch), Permission::Write)
            .await?;
        if force {
            self.driver.force_stop(&branch.to_string()).await?;
        } else {
            self.driver.stop(&branch.to_string()).await?;
        }
        self.runtime.sandbox_pins.lock().remove(&branch);
        if let Some(binding) = self.runtime.state.mounts().await?.into_iter().find(|m| {
            m.branch == branch && m.state == MountState::Ready && m.access == AccessMode::ReadWrite
        }) {
            let saved = self
                .services
                .mounts
                .save(
                    RequestContext {
                        principal: actor.clone(),
                        request_id: format!("sandbox-stop-{}", OperationId::new()),
                    },
                    SaveMount {
                        message: Some("Save Sandbox files".into()),
                        mount: binding.id,
                        expected_binding_generation: binding.generation,
                        durability: Durability::Local,
                        wait_for_remote_ms: None,
                    },
                )
                .await?;
            if let Some(error) = saved.error {
                return Err(error);
            }
            if !saved.local_saved {
                return Err(Error::new(
                    ErrorCode::Busy,
                    "Sandbox 已停止，但分支保存尚未完成，请稍后重试保存",
                ));
            }
        }
        self.driver.status(&branch.to_string()).await
    }
}

struct PendingPin {
    runtime: Arc<Runtime>,
    branch: BranchId,
    retained: bool,
}
impl Drop for PendingPin {
    fn drop(&mut self) {
        if !self.retained {
            self.runtime.sandbox_pins.lock().remove(&self.branch);
        }
    }
}

impl SandboxService {
    async fn create_inner(
        &self,
        actor: &Principal,
        workspace: WorkspaceId,
        branch: BranchId,
        template_id: Option<String>,
    ) -> Result<SandboxStatus> {
        self.runtime
            .authorize(actor, workspace, Some(branch), Permission::Write)
            .await?;
        if let Some(id) = &template_id
            && !self
                .driver
                .templates()
                .await?
                .iter()
                .any(|item| &item.id == id)
        {
            return Err(Error::invalid("模板不存在"));
        }
        let id = branch.to_string();
        let existing_template = self
            .driver
            .status(&id)
            .await
            .map(|s| s.template_id)
            .unwrap_or_else(|_| default_sandbox_template());
        if let Ok(existing) = self.driver.status(&id).await
            && template_id
                .as_ref()
                .is_some_and(|id| id != &existing.template_id)
        {
            return Err(Error::new(
                ErrorCode::Busy,
                "已有 Sandbox 使用其他模板，请删除后重新创建",
            ));
        }
        match self.driver.status(&id).await {
            Ok(status)
                if matches!(
                    status.state,
                    SandboxState::Ready | SandboxState::Starting | SandboxState::Creating
                ) =>
            {
                return Ok(status);
            }
            Err(error) if error.code != ErrorCode::NotFound => return Err(error),
            _ => {}
        }
        self.driver
            .report_progress(SandboxStatus {
                template_id: template_id
                    .clone()
                    .unwrap_or_else(|| existing_template.clone()),
                services: Vec::new(),
                id: id.clone(),
                workspace,
                branch,
                state: SandboxState::Creating,
                desktop_url: None,
                message: None,
                startup_step: Some("prepare_branch".into()),
            })
            .await;
        let value = self.runtime.branch(branch).await?;
        if value.owner != self.runtime.location || value.state != BranchState::Writable {
            return Err(Error::new(
                ErrorCode::BranchBusy,
                "Sandbox requires local branch write authority",
            ));
        }
        let binding = self.runtime.state.mounts().await?.into_iter().find(|m| {
            m.branch == branch
                && m.access == AccessMode::ReadWrite
                && m.state != MountState::Released
        });
        if binding.is_none() {
            self.services
                .mounts
                .prepare(
                    RequestContext {
                        principal: actor.clone(),
                        request_id: format!("sandbox-mount-{id}-{}", OperationId::new()),
                    },
                    PrepareMount {
                        automatic: false,
                        workspace,
                        branch: Some(branch),
                        revision: None,
                        path: self.mount_root.join(&id),
                        access: AccessMode::ReadWrite,
                        durability: Durability::Local,
                    },
                )
                .await?;
        }
        let _lifecycle = self.runtime.lifecycle.lock().await;
        let binding = self
            .runtime
            .state
            .mounts()
            .await?
            .into_iter()
            .find(|m| {
                m.branch == branch
                    && m.access == AccessMode::ReadWrite
                    && m.state == MountState::Ready
            })
            .ok_or_else(|| Error::new(ErrorCode::MountBusy, "Branch mount is not ready"))?;
        {
            let _branch = self.runtime.lock_branch(branch).await;
            let current = self.runtime.branch(branch).await?;
            current.check_write(self.runtime.location, value.authority_epoch)?;
            if binding
                .branch_generation
                .is_some_and(|generation| generation != current.generation)
            {
                return Err(Error::new(
                    ErrorCode::StaleBinding,
                    "Branch binding changed before Sandbox startup",
                ));
            }
            self.runtime.sandbox_pins.lock().insert(branch);
        }
        let mut pin = PendingPin {
            runtime: self.runtime.clone(),
            branch,
            retained: false,
        };
        match self
            .driver
            .start(SandboxLaunch {
                id,
                binding,
                template_id,
            })
            .await
        {
            Ok(status) => {
                pin.retained = true;
                Ok(status)
            }
            Err(error) => {
                self.runtime.sandbox_pins.lock().remove(&branch);
                Err(error)
            }
        }
    }
}
