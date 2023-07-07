//! utils for deploy and control remote machines
use anyhow::{anyhow, Context, Result};
use std::path::Path;
use std::process::{Command, Output};

use super::Host;

/// execute remote ssh
pub fn timeout_ssh(host: &Host, command: &[&str], learn_known_host_key: bool) -> Result<Output> {
    let target = host.deploy_ssh_target();
    let mut args = vec!["-o", "ConnectTimeout=10", "-o", "StrictHostKeyChecking=no"];
    if !learn_known_host_key {
        args.push("-o");
        args.push("UserKnownHostsFile=/dev/null");
    }
    args.push(&target);
    args.push("--");
    args.extend(command);
    println!("$ ssh {}", args.join(" "));
    let output = Command::new("ssh")
        .args(args)
        .output()
        .context("Failed to run ssh...")?;
    Ok(output)
}

/// execute ssh remote copy
pub fn timeout_scp(host: &Host, src: String, dst: &Path) -> Result<()> {
    let target = host.deploy_ssh_target();
    let args = vec![
        format!("{target}:{src}"),
        dst.to_str()
            .ok_or(anyhow!("destination of scp is invalid"))?
            .into(),
    ];
    println!("$ scp {}", args.join(" "));
    Command::new("scp")
        .args(args)
        .spawn()
        .context("Failed to run scp...")?
        .wait()
        .context("Failed to wait on scp child")?;
    Ok(())
}
