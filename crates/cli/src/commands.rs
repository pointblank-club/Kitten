//! The commands run in the normal system. The work happens in `engine::service`; this module
//! reads arguments and prints results.

use std::path::{Path, PathBuf};

use anyhow::{Context, bail};
use bootshift_core::layout::{Layout, Target};
use bootshift_core::plan::Plan;
use bootshift_core::refusal::Refusal;
use bootshift_core::units::format_bytes;
use bootshift_engine::preflight;
use bootshift_engine::service::{self, Inspected, ServiceError, SystemPaths};
use bootshift_fs::run::SystemRunner;

pub fn probe(disk: &Path, target_template: bool) -> anyhow::Result<()> {
    let inspected = inspect(disk)?;
    if target_template {
        let target = service::current_target(&inspected.layout);
        println!("{}", serde_json::to_string_pretty(&target)?);
    } else {
        print_layout(&inspected);
    }
    Ok(())
}

pub fn plan(disk: &Path, target: &Path) -> anyhow::Result<()> {
    let inspected = inspect(disk)?;
    let plan = planned(&inspected.layout, target)?;
    print_plan(&inspected.layout, &plan);
    Ok(())
}

pub fn check(disk: &Path, target: &Path) -> anyhow::Result<()> {
    let inspected = inspect(disk)?;
    let plan = planned(&inspected.layout, target)?;
    let (_, refusals) =
        service::run_checks(&SystemRunner, &SystemPaths::default(), &inspected, &plan);
    if !refusals.is_empty() {
        print_refusals(&refusals);
        bail!("{} check(s) failed", refusals.len());
    }
    println!("Every check passed.");
    Ok(())
}

pub fn apply(
    disk: &Path,
    target: &Path,
    confirmed: bool,
    init: Option<PathBuf>,
) -> anyhow::Result<()> {
    let inspected = inspect(disk)?;
    let plan = planned(&inspected.layout, target)?;
    if plan.operations.is_empty() {
        println!("The disk already has this layout.");
        return Ok(());
    }
    print_plan(&inspected.layout, &plan);
    if !confirmed {
        println!(
            "Nothing has changed. Make sure your data is backed up, then run again with --yes."
        );
        return Ok(());
    }

    let init = match init {
        Some(path) => path,
        None => std::env::current_exe().context("finding the bootshift binary")?,
    };
    let work_dir = std::env::temp_dir().join(format!("bootshift-{}", std::process::id()));
    // Create it private (0o700) and fail closed on a pre-existing directory: this holds the recovery
    // initramfs, so a predictable name in a shared /tmp must not be pre-created or symlinked by
    // another user to capture it. (Mirrors the daemon's apply path.)
    {
        use std::os::unix::fs::DirBuilderExt;
        std::fs::DirBuilder::new()
            .recursive(false)
            .mode(0o700)
            .create(&work_dir)
            .with_context(|| format!("creating a private work directory {}", work_dir.display()))?;
    }
    let result = service::apply(
        &SystemRunner,
        &SystemPaths::default(),
        &inspected,
        &plan,
        &init,
        &work_dir,
    );
    let _ = std::fs::remove_dir_all(&work_dir);
    result.map_err(explain)?;

    println!("Everything is ready. Restart the computer to begin.");
    println!("If the power goes out during the change, just turn the computer back on.");
    Ok(())
}

fn inspect(disk: &Path) -> anyhow::Result<Inspected> {
    service::inspect(&SystemRunner, disk).map_err(explain)
}

fn planned(layout: &Layout, target: &Path) -> anyhow::Result<Plan> {
    let text =
        std::fs::read_to_string(target).with_context(|| format!("reading {}", target.display()))?;
    let target: Target = serde_json::from_str(&text)
        .with_context(|| format!("{} is not a target layout", target.display()))?;
    service::plan_target(layout, &target).map_err(explain)
}

/// Prints refusals with their fixes, and turns the error into one for the exit status.
fn explain(error: ServiceError) -> anyhow::Error {
    if let ServiceError::Refused(refusals) = &error {
        print_refusals(refusals);
        return anyhow::anyhow!("{} check(s) failed, so nothing was changed", refusals.len());
    }
    error.into()
}

fn print_layout(inspected: &Inspected) {
    let layout = &inspected.layout;
    let sector = layout.geometry.sector_size;
    println!(
        "{}: {}, {sector}-byte sectors",
        inspected.disk.path.display(),
        format_bytes(inspected.disk.size)
    );
    println!(
        "{:>3}  {:<20} {:>10} {:>10}  {:<13} {:>10} {:>14}",
        "#", "Name", "Start", "Size", "Contents", "Used", "Can shrink to"
    );
    for p in &layout.partitions {
        let min = p
            .min_len
            .map_or("-".to_string(), |s| format_bytes(s.to_bytes(sector)));
        println!(
            "{:>3}  {:<20} {:>10} {:>10}  {:<13} {:>10} {:>14}",
            p.number,
            p.name.chars().take(20).collect::<String>(),
            format_bytes(p.extent.start.0 * u64::from(sector)),
            format_bytes(p.extent.len.to_bytes(sector)),
            p.content.name(),
            format_bytes(p.used.to_bytes(sector)),
            min
        );
    }
}

fn print_plan(layout: &Layout, plan: &Plan) {
    if plan.operations.is_empty() {
        println!("The disk already has this layout.");
        return;
    }
    for (index, operation) in plan.operations.iter().enumerate() {
        println!("{:>2}. {}", index + 1, operation.describe(layout));
    }
    let sector = layout.geometry.sector_size;
    println!(
        "Copies about {} in total, during one restart.",
        format_bytes(plan.sectors_to_copy().to_bytes(sector))
    );
    let unavailable = preflight::unavailable_operations(layout, plan);
    if !unavailable.is_empty() {
        print_refusals(&unavailable);
    }
}

fn print_refusals(refusals: &[Refusal]) {
    for refusal in refusals {
        eprintln!("✗ {}\n  → {}", refusal.reason, refusal.fix);
    }
}
