use super::*;
use std::time::Duration;

async fn terminate(child: &mut Child, force: bool, timeout: Duration) -> Result<()> {
    if child.try_wait()?.is_some() {
        return Ok(());
    }
    if !force {
        let stopped = tokio::time::timeout(timeout, async {
            if let Some(stdin) = child.stdin.as_mut() {
                stdin.write_all(b"stop\n").await?;
            }
            child.wait().await
        })
        .await;
        if matches!(stopped, Ok(Ok(_))) {
            return Ok(());
        }
    }
    child.start_kill()?;
    tokio::time::timeout(timeout, child.wait())
        .await
        .map_err(|_| {
            Error::new(
                ErrorCode::Unavailable,
                "等待 Sandbox 进程退出超时，请稍后重试",
            )
        })??;
    Ok(())
}

impl NativeSandbox {
    pub(super) async fn stop_instance(&self, id: &str, force: bool) -> Result<()> {
        let mut instances = self.instances.lock().await;
        if let Some(instance) = instances.get_mut(id) {
            if !force && instance.child.try_wait()?.is_none() {
                let flushed = tokio::time::timeout(Duration::from_secs(30), async {
                    let mut stream = self.guest(id, 7002).await?;
                    let mut response = String::new();
                    stream.read_line(&mut response).await?;
                    let value: serde_json::Value = serde_json::from_str(&response)?;
                    if value.get("flushed").and_then(|value| value.as_bool()) != Some(true) {
                        return Err(Error::new(ErrorCode::Io, "Sandbox 未确认文件同步"));
                    }
                    Ok::<_, Error>(())
                })
                .await;
                let error = match flushed {
                    Ok(Ok(())) => None,
                    Ok(Err(error)) => Some(error.message),
                    Err(_) => Some("文件同步超时".into()),
                };
                if let Some(error) = error {
                    instance.status.message =
                        Some(format!("正常停止失败：{error}。可以重试，或使用强制停止。"));
                    return Err(Error::new(
                        ErrorCode::Unavailable,
                        instance.status.message.clone().unwrap(),
                    ));
                }
            }
            terminate(&mut instance.child, force, Duration::from_secs(5)).await?;
            instance.proxy.take();
            instance.status.state = SandboxState::Stopped;
            instance.status.desktop_url = None;
            instance.status.startup_step = None;
            instance.status.message =
                force.then(|| "已强制停止；guest 中尚未同步的修改可能丢失。".into());
            self.progress.lock().await.remove(id);
            tokio::fs::write(
                self.config.directory.join(id).join("status.json"),
                serde_json::to_vec_pretty(&instance.status)?,
            )
            .await?;
            return Ok(());
        }
        let directory = self.config.directory.join(id);
        if directory.join("status.json").is_file() {
            self.progress.lock().await.remove(id);
            return Ok(());
        }
        Err(Error::new(ErrorCode::NotFound, "Sandbox does not exist"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn unresponsive_worker_is_killed_and_reaped_after_timeout() {
        let mut child = Command::new("sleep")
            .arg("30")
            .stdin(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        terminate(&mut child, false, Duration::from_millis(50))
            .await
            .unwrap();
        assert!(child.try_wait().unwrap().is_some());
    }
}
