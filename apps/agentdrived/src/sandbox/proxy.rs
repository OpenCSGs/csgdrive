use super::*;
use std::path::Path;
use tokio::net::{TcpStream, UnixListener};

pub(super) fn validate(config: &SandboxProxyConfig) -> Result<Option<(String, u16)>> {
    let Some(value) = &config.url else {
        return Ok(None);
    };
    let uri: axum::http::Uri = value.parse().map_err(|_| Error::invalid("代理地址无效"))?;
    if uri.scheme_str() != Some("http")
        || uri.authority().is_none()
        || uri.authority().unwrap().as_str().contains('@')
        || uri
            .path_and_query()
            .is_some_and(|path| path.as_str() != "/")
    {
        return Err(Error::invalid(
            "请输入不含账号、密码或路径的 HTTP 代理地址，例如 http://127.0.0.1:7890",
        ));
    }
    Ok(Some((
        uri.host().unwrap().trim_matches(['[', ']']).into(),
        uri.port_u16().unwrap_or(80),
    )))
}

pub(super) async fn read_config(path: &Path) -> Result<SandboxProxyConfig> {
    match tokio::fs::read(path).await {
        Ok(bytes) => Ok(serde_json::from_slice(&bytes)?),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            Ok(SandboxProxyConfig::default())
        }
        Err(error) => Err(error.into()),
    }
}

impl NativeSandbox {
    pub(super) fn proxy_path(&self, id: &str) -> Result<PathBuf> {
        let branch: BranchId = id.parse().map_err(|_| Error::invalid("Sandbox 分支无效"))?;
        if branch.to_string() != id {
            return Err(Error::invalid("Sandbox 分支无效"));
        }
        Ok(self
            .config
            .directory
            .parent()
            .ok_or_else(|| Error::invalid("实例目录缺少父目录"))?
            .join("proxy-settings")
            .join(format!("{id}.json")))
    }
}

// 专用 vsock 入口只转发到本机用户配置的代理，不开放宿主机网络访问。
#[derive(Debug)]
pub(super) struct ProxyRelay(tokio::task::JoinHandle<()>);
impl Drop for ProxyRelay {
    fn drop(&mut self) {
        self.0.abort();
    }
}
impl ProxyRelay {
    pub(super) async fn start(directory: &Path, config_path: PathBuf) -> Result<Self> {
        let target = validate(&read_config(&config_path).await?)?;
        let path = directory.join("vsock.sock_18081");
        match tokio::fs::remove_file(&path).await {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        let listener = UnixListener::bind(path)?;
        Ok(Self(tokio::spawn(async move {
            let mut connections = tokio::task::JoinSet::new();
            loop {
                tokio::select! {
                    result = listener.accept(), if connections.len() < 64 => {
                        let Ok((mut guest, _)) = result else { break };
                        let Some((host, port)) = target.clone() else { continue };
                        connections.spawn(async move {
                            if let Ok(Ok(mut upstream)) = tokio::time::timeout(
                                std::time::Duration::from_secs(10), TcpStream::connect((host.as_str(), port))
                            ).await {
                                let _ = tokio::io::copy_bidirectional(&mut guest, &mut upstream).await;
                            }
                        });
                    }
                    _ = connections.join_next(), if !connections.is_empty() => {},
                }
            }
        })))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    #[tokio::test]
    async fn relay_preserves_connect_bytes_and_stops_with_the_instance() {
        let directory = tempfile::tempdir().unwrap();
        let upstream = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let config = directory.path().join("proxy.json");
        tokio::fs::write(
            &config,
            serde_json::to_vec(&SandboxProxyConfig {
                url: Some(format!("http://{}", upstream.local_addr().unwrap())),
            })
            .unwrap(),
        )
        .await
        .unwrap();
        let relay = ProxyRelay::start(directory.path(), config).await.unwrap();
        let mut guest = tokio::net::UnixStream::connect(directory.path().join("vsock.sock_18081"))
            .await
            .unwrap();
        guest
            .write_all(b"CONNECT example.com:443 HTTP/1.1\r\n\r\n")
            .await
            .unwrap();
        let (mut remote, _) = upstream.accept().await.unwrap();
        let mut request = [0; 36];
        remote.read_exact(&mut request).await.unwrap();
        assert_eq!(&request, b"CONNECT example.com:443 HTTP/1.1\r\n\r\n");
        remote.write_all(b"HTTP/1.1 200 OK\r\n\r\n").await.unwrap();
        let mut response = [0; 19];
        guest.read_exact(&mut response).await.unwrap();
        assert_eq!(&response, b"HTTP/1.1 200 OK\r\n\r\n");
        drop(relay);
        assert_eq!(
            tokio::time::timeout(std::time::Duration::from_secs(2), guest.read(&mut response))
                .await
                .unwrap()
                .unwrap(),
            0
        );
    }

    #[test]
    fn proxy_configuration_accepts_only_an_http_proxy_authority() {
        for value in [
            "socks5://localhost:7890",
            "http://user:secret@localhost:7890",
            "http://localhost/path",
            "http://localhost/?x=1",
            "file:///tmp/proxy",
        ] {
            assert!(
                validate(&SandboxProxyConfig {
                    url: Some(value.into())
                })
                .is_err()
            );
        }
        assert_eq!(
            validate(&SandboxProxyConfig {
                url: Some("http://127.0.0.1:7890".into())
            })
            .unwrap(),
            Some(("127.0.0.1".into(), 7890))
        );
        assert!(validate(&SandboxProxyConfig::default()).unwrap().is_none());
    }
}
