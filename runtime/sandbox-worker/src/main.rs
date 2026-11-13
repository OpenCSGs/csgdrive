use cube_hypervisor::{vm_config::VmConfig, vmm_config::VmmConfig, ApiRequest, VmmInstance};
use std::io::{self, BufRead, Write};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let path = std::env::args()
        .nth(1)
        .ok_or("expected VM configuration path")?;
    let directory = std::path::Path::new(&path)
        .parent()
        .ok_or("missing instance directory")?;
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .open(directory.join("worker.lock"))?;
    lock.try_lock()?;
    let config: VmConfig = serde_json::from_reader(std::fs::File::open(path)?)?;
    let vmm = VmmInstance::new(VmmConfig {
        log_stderr: true,
        log_json_file: None,
        ..Default::default()
    })?;
    vmm.send_request(ApiRequest::VmCreate(Box::new(config)))
        .map_err(|e| format!("{e:?}"))?
        .map_err(|e| format!("{e:?}"))?;
    vmm.send_request(ApiRequest::VmBoot)
        .map_err(|e| format!("{e:?}"))?
        .map_err(|e| format!("{e:?}"))?;
    println!("{{\"state\":\"booting\"}}");
    io::stdout().flush()?;
    // Closing the parent channel terminates this instance and releases its resources.
    for command in io::stdin().lock().lines() {
        match command?.as_str() {
            "stop" => break,
            "ping" => {
                vmm.send_request(ApiRequest::VmmPing)
                    .map_err(|e| format!("{e:?}"))?
                    .map_err(|e| format!("{e:?}"))?;
                println!("{{\"state\":\"alive\"}}");
                io::stdout().flush()?;
            }
            _ => return Err("unknown worker command".into()),
        }
    }
    vmm.send_request(ApiRequest::VmDelete)
        .map_err(|e| format!("{e:?}"))?
        .map_err(|e| format!("{e:?}"))?;
    Ok(())
}
