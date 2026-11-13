use super::*;

impl MountService {
    pub(crate) async fn suspend_native_mounts(&self) -> Result<()> {
        let _lifecycle = self.runtime.lifecycle.lock().await;
        let mut failure = None;
        for binding in self.runtime.state.mounts().await? {
            if matches!(binding.state, MountState::Suspended | MountState::Released) {
                continue;
            }
            if let Err(error) = self.suspend_native(binding).await {
                failure.get_or_insert(error);
            }
        }
        failure.map_or(Ok(()), Err)
    }

    pub(crate) async fn restore_native_mounts(&self) -> Result<()> {
        let _lifecycle = self.runtime.lifecycle.lock().await;
        for binding in self.runtime.state.mounts().await? {
            if binding.state == MountState::Suspended
                && let Err(error) = self.install_new(&binding).await
            {
                tracing::warn!(mount = %binding.id, path = %binding.path.display(), %error, "could not restore native mount");
                let mut failed = binding;
                failed.state = MountState::RecoveryRequired;
                self.runtime
                    .state
                    .commit(LocalCommit {
                        mounts: vec![failed],
                        ..Default::default()
                    })
                    .await?;
            }
        }
        Ok(())
    }

    async fn suspend_native(&self, mut binding: MountBinding) -> Result<()> {
        match self.driver.status(&binding).await? {
            NativeMountState::Mounted => {
                self.driver.unmount(&binding).await.map_err(|mut error| {
                    error.message = format!("{}: {}", binding.path.display(), error.message);
                    error
                })?
            }
            NativeMountState::Absent => (),
            NativeMountState::Unknown => {
                // A native rebind may have succeeded just before persisting its new generation.
                // Only detach a replacement positively identified by the driver and journal.
                for operation in self.runtime.state.operations(binding.workspace).await? {
                    let Some(
                        PendingAction::MountChange {
                            new: Some(next), ..
                        }
                        | PendingAction::BranchSwitch {
                            new: Some(next), ..
                        },
                    ) = operation.pending
                    else {
                        continue;
                    };
                    if next.id == binding.id
                        && next.path == binding.path
                        && self.driver.status(&next).await? == NativeMountState::Mounted
                    {
                        self.driver.unmount(&next).await?;
                        break;
                    }
                }
                binding.state = MountState::RecoveryRequired;
                return self
                    .runtime
                    .state
                    .commit(LocalCommit {
                        mounts: vec![binding],
                        ..Default::default()
                    })
                    .await;
            }
        }
        if binding.state == MountState::Ready {
            binding.state = MountState::Suspended;
            self.runtime
                .state
                .commit(LocalCommit {
                    mounts: vec![binding],
                    ..Default::default()
                })
                .await?;
        }
        Ok(())
    }
}
