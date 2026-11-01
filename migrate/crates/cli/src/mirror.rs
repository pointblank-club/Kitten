//! LVM mirror mode (implementation I5.5, plan Phase 9): instead of shipping blocks, the receiver
//! exports its target PV over nvme-tcp, the sender's kernel adds it to the source volume group as a
//! raid1 leg, and LVM mirrors every write to both legs synchronously. At cutover, once the legs are
//! in sync, the source freezes and stands down and the target promotes its leg to a standalone
//! volume group. This is the M1 exception: mirror mode writes the source's LVM metadata, so it runs
//! only with explicit consent.
//!
//! Each action is a thin, idempotent wrapper over the standard tools (nvme, nvmet via configfs,
//! lvm). The two-node ordering (sync, freeze, stand down, promote) is driven by the caller.

use std::path::Path;
use std::process::Command;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};

const NVMET: &str = "/sys/kernel/config/nvmet";

/// Receiver: export `dev` (the target PV partition) over nvme-tcp under `nqn`, listening on
/// `listen:port`. The export lives in configfs, so it persists after this process returns.
///
/// `host_nqn` is the initiator (sender) NQN permitted to connect. When given, only that host may
/// attach; when `None`, the export is readable AND writable by any host that can reach the port,
/// which exposes every raw block of the migrating disk, so that path warns loudly.
pub fn export(dev: &str, listen: &str, port: u16, nqn: &str, host_nqn: Option<&str>) -> Result<()> {
    ensure_configfs()?;
    let subsystem = format!("{NVMET}/subsystems/{nqn}");
    mkdir(&subsystem)?;
    match host_nqn {
        Some(host) => {
            // Restrict to the one initiator: register it under hosts/ and link it into the
            // subsystem's allowed_hosts, with any-host access turned off.
            mkdir(&format!("{NVMET}/hosts/{host}"))?;
            let allowed = format!("{subsystem}/allowed_hosts/{host}");
            if !Path::new(&allowed).exists() {
                std::os::unix::fs::symlink(format!("{NVMET}/hosts/{host}"), &allowed)
                    .with_context(|| format!("allowing host {host} at {allowed}"))?;
            }
            write(&format!("{subsystem}/attr_allow_any_host"), "0")?;
        }
        None => {
            write(&format!("{subsystem}/attr_allow_any_host"), "1")?;
            eprintln!(
                "mirror: WARNING: exporting {dev} with no host restriction; any host that can reach \
                 {listen}:{port} can read and write the target disk. Pass --host-nqn <initiator> to \
                 restrict it."
            );
        }
    }
    let namespace = format!("{subsystem}/namespaces/1");
    mkdir(&namespace)?;
    write(&format!("{namespace}/device_path"), dev)?;
    write(&format!("{namespace}/enable"), "1")?;

    let port_dir = format!("{NVMET}/ports/{port}");
    mkdir(&port_dir)?;
    write(&format!("{port_dir}/addr_adrfam"), "ipv4")?;
    write(&format!("{port_dir}/addr_trtype"), "tcp")?;
    write(&format!("{port_dir}/addr_traddr"), listen)?;
    write(&format!("{port_dir}/addr_trsvcid"), &port.to_string())?;
    let link = format!("{port_dir}/subsystems/{nqn}");
    if !Path::new(&link).exists() {
        std::os::unix::fs::symlink(&subsystem, &link)
            .with_context(|| format!("linking subsystem into port at {link}"))?;
    }
    println!("mirror: exporting {dev} as {nqn} over tcp on {listen}:{port}");
    Ok(())
}

/// Receiver: remove the nvme-tcp export.
pub fn teardown(nqn: &str, port: u16) -> Result<()> {
    let _ = std::fs::remove_file(format!("{NVMET}/ports/{port}/subsystems/{nqn}"));
    let namespace = format!("{NVMET}/subsystems/{nqn}/namespaces/1");
    let _ = write(&format!("{namespace}/enable"), "0");
    let _ = std::fs::remove_dir(&namespace);
    let _ = std::fs::remove_dir(format!("{NVMET}/ports/{port}"));
    let _ = std::fs::remove_dir(format!("{NVMET}/subsystems/{nqn}"));
    println!("mirror: export {nqn} torn down");
    Ok(())
}

/// Sender: connect the exported target, add it to `vg` as a raid1 leg of `lv`, and wait until the
/// legs are in sync. The source filesystem stays mounted and live throughout.
pub fn attach(addr: &str, port: u16, nqn: &str, vg: &str, lv: &str) -> Result<()> {
    let before = nvme_namespaces();
    run(
        Command::new("nvme").args([
            "connect",
            "-t",
            "tcp",
            "-a",
            addr,
            "-s",
            &port.to_string(),
            "-n",
            nqn,
        ]),
        "nvme connect",
    )?;
    let dev = wait_for_new_namespace(&before, Duration::from_secs(15))?;
    println!("mirror: connected target leg as {dev}");

    run(Command::new("pvcreate").args(["-y", &dev]), "pvcreate")?;
    run(Command::new("vgextend").args([vg, &dev]), "vgextend")?;
    run(
        Command::new("lvconvert").args([
            "-y",
            "--type",
            "raid1",
            "-m",
            "1",
            "--regionsize",
            "4M",
            &format!("{vg}/{lv}"),
            &dev,
        ]),
        "lvconvert raid1",
    )?;
    wait_sync(vg, lv, Duration::from_secs(900))?;
    println!("mirror: legs in sync (100%)");
    Ok(())
}

/// Sender: the cutover stand-down. The caller has already frozen and unmounted the source
/// filesystem. Deactivate the volume group and drop the target leg, so the target can take over
/// with no second live copy (M7).
pub fn stand_down(vg: &str, nqn: &str) -> Result<()> {
    run(Command::new("vgchange").args(["-an", vg]), "vgchange -an")?;
    let _ = Command::new("nvme")
        .args(["disconnect", "-n", nqn])
        .status();
    println!("mirror: source stood down (volume group deactivated, target leg released)");
    Ok(())
}

/// Sender: roll back the mirror — drop the target leg, keep the source leg (which has the data all
/// along, M1), remove the target PV and disconnect it. The source is left exactly as it was.
pub fn rollback(vg: &str, lv: &str, nqn: &str) -> Result<()> {
    let dev = any_nvme_namespace().context("finding the connected target leg to roll back")?;
    run(
        Command::new("lvconvert").args(["-y", "-m", "0", &format!("{vg}/{lv}"), &dev]),
        "lvconvert -m 0 (drop target leg)",
    )?;
    run(Command::new("vgreduce").args([vg, &dev]), "vgreduce")?;
    let _ = Command::new("nvme")
        .args(["disconnect", "-n", nqn])
        .status();
    println!("mirror: rolled back; the source keeps its data and the target leg is gone");
    Ok(())
}

/// Receiver: promote the mirror leg to a standalone volume group — drop the now-missing source PV,
/// collapse the raid to the single local leg, and activate it. The target now owns the data.
pub fn promote(vg: &str, lv: &str) -> Result<()> {
    run(
        Command::new("vgreduce").args(["--removemissing", "--force", vg]),
        "vgreduce --removemissing",
    )?;
    run(
        Command::new("lvconvert").args(["-y", "-m", "0", &format!("{vg}/{lv}")]),
        "lvconvert -m 0",
    )?;
    run(Command::new("vgchange").args(["-ay", vg]), "vgchange -ay")?;
    println!("mirror: promoted {vg}/{lv} on the target");
    Ok(())
}

fn wait_sync(vg: &str, lv: &str, timeout: Duration) -> Result<()> {
    let deadline = Instant::now() + timeout;
    loop {
        let output = Command::new("lvs")
            .args(["--noheadings", "-o", "sync_percent", &format!("{vg}/{lv}")])
            .output()
            .context("running lvs to read sync_percent")?;
        let percent = String::from_utf8_lossy(&output.stdout)
            .trim()
            .trim_end_matches('%')
            .trim()
            .to_string();
        if percent == "100.00" {
            return Ok(());
        }
        if Instant::now() >= deadline {
            bail!("the mirror did not reach 100% in time (last {percent}%)");
        }
        std::thread::sleep(Duration::from_secs(1));
    }
}

fn nvme_namespaces() -> Vec<String> {
    let mut found = Vec::new();
    if let Ok(entries) = std::fs::read_dir("/dev") {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if is_namespace_node(&name) {
                found.push(format!("/dev/{name}"));
            }
        }
    }
    found.sort();
    found
}

/// Whether `name` is an nvme namespace node (`nvme1n1`), not a controller node (`nvme1`).
fn is_namespace_node(name: &str) -> bool {
    let Some(rest) = name.strip_prefix("nvme") else {
        return false;
    };
    let mut parts = rest.splitn(2, 'n');
    let controller = parts.next().unwrap_or("");
    let namespace = parts.next().unwrap_or("");
    !controller.is_empty()
        && controller.chars().all(|c| c.is_ascii_digit())
        && !namespace.is_empty()
        && namespace.chars().all(|c| c.is_ascii_digit())
}

fn any_nvme_namespace() -> Result<String> {
    nvme_namespaces()
        .into_iter()
        .next()
        .context("no connected nvme target namespace found")
}

fn wait_for_new_namespace(before: &[String], timeout: Duration) -> Result<String> {
    let deadline = Instant::now() + timeout;
    loop {
        let now = nvme_namespaces();
        if let Some(new) = now.iter().find(|d| !before.contains(d)) {
            return Ok(new.clone());
        }
        if Instant::now() >= deadline {
            bail!("the connected nvme target did not appear as a device");
        }
        std::thread::sleep(Duration::from_millis(200));
    }
}

fn ensure_configfs() -> Result<()> {
    if Path::new(NVMET).exists() {
        return Ok(());
    }
    let status = Command::new("mount")
        .args(["-t", "configfs", "none", "/sys/kernel/config"])
        .status();
    if Path::new(NVMET).exists() {
        return Ok(());
    }
    bail!("nvmet configfs is not available (is the nvmet module loaded?); mount said {status:?}");
}

fn mkdir(path: &str) -> Result<()> {
    if Path::new(path).exists() {
        return Ok(());
    }
    std::fs::create_dir_all(path).with_context(|| format!("creating {path}"))
}

fn write(path: &str, value: &str) -> Result<()> {
    std::fs::write(path, value).with_context(|| format!("writing {value:?} to {path}"))
}

fn run(command: &mut Command, label: &str) -> Result<()> {
    let output = command
        .output()
        .with_context(|| format!("running {label}"))?;
    if !output.status.success() {
        bail!(
            "{label} failed:\n{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }
    Ok(())
}
