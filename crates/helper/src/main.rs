//! `bootshiftd`: the root service the GUI talks to over the system D-Bus. It re-plans and
//! re-checks everything it is asked to do, and never trusts a plan from the caller.

mod authority;
mod responses;

use std::path::{Path, PathBuf};

use bootshift_engine::service::{self, ServiceError, SystemPaths};
use bootshift_fs::run::SystemRunner;
use zbus::fdo;
use zbus::message::Header;

const BUS_NAME: &str = "org.bootshift.Helper1";
const OBJECT_PATH: &str = "/org/bootshift/Helper1";
/// The static `bootshift` binary that becomes the recovery system's init.
const RECOVERY_BINARY: &str = "/usr/libexec/bootshift/bootshift";

struct Helper {
    recovery_binary: PathBuf,
}

#[zbus::interface(name = "org.bootshift.Helper1")]
impl Helper {
    fn version(&self) -> String {
        env!("CARGO_PKG_VERSION").to_string()
    }

    async fn list_disks(
        &self,
        #[zbus(header)] header: Header<'_>,
        #[zbus(connection)] connection: &zbus::Connection,
    ) -> fdo::Result<String> {
        authority::require(connection, &header, authority::INSPECT).await?;
        run_blocking(|| {
            let disks = bootshift_fs::probe::disks(&SystemRunner).map_err(ServiceError::from);
            responses::reply(disks.map(|d| responses::disk_summaries(&d)))
        })
        .await
    }

    async fn probe(
        &self,
        #[zbus(header)] header: Header<'_>,
        #[zbus(connection)] connection: &zbus::Connection,
        disk: String,
    ) -> fdo::Result<String> {
        authority::require(connection, &header, authority::INSPECT).await?;
        run_blocking(move || responses::reply(probe(&disk))).await
    }

    async fn plan(
        &self,
        #[zbus(header)] header: Header<'_>,
        #[zbus(connection)] connection: &zbus::Connection,
        disk: String,
        target: String,
    ) -> fdo::Result<String> {
        authority::require(connection, &header, authority::INSPECT).await?;
        run_blocking(move || responses::reply(plan(&disk, &target))).await
    }

    async fn check(
        &self,
        #[zbus(header)] header: Header<'_>,
        #[zbus(connection)] connection: &zbus::Connection,
        disk: String,
        target: String,
    ) -> fdo::Result<String> {
        authority::require(connection, &header, authority::INSPECT).await?;
        run_blocking(move || responses::reply(check(&disk, &target))).await
    }

    async fn apply(
        &self,
        #[zbus(header)] header: Header<'_>,
        #[zbus(connection)] connection: &zbus::Connection,
        disk: String,
        target: String,
    ) -> fdo::Result<String> {
        authority::require(connection, &header, authority::APPLY).await?;
        let recovery_binary = self.recovery_binary.clone();
        run_blocking(move || responses::reply(apply(&disk, &target, &recovery_binary))).await
    }
}

fn probe(disk: &str) -> Result<responses::Probed, ServiceError> {
    let inspected = service::inspect(&SystemRunner, Path::new(disk))?;
    Ok(responses::probed(&inspected))
}

fn plan(disk: &str, target: &str) -> Result<responses::Planned, ServiceError> {
    let inspected = service::inspect(&SystemRunner, Path::new(disk))?;
    responses::planned(&inspected.layout, &responses::parse_target(target)?)
}

fn check(disk: &str, target: &str) -> Result<(), ServiceError> {
    let inspected = service::inspect(&SystemRunner, Path::new(disk))?;
    let plan = service::plan_target(&inspected.layout, &responses::parse_target(target)?)?;
    let (_, refusals) =
        service::run_checks(&SystemRunner, &SystemPaths::default(), &inspected, &plan);
    if refusals.is_empty() {
        Ok(())
    } else {
        Err(ServiceError::Refused(refusals))
    }
}

fn apply(
    disk: &str,
    target: &str,
    recovery_binary: &Path,
) -> Result<responses::Applied, ServiceError> {
    let inspected = service::inspect(&SystemRunner, Path::new(disk))?;
    let plan = service::plan_target(&inspected.layout, &responses::parse_target(target)?)?;
    // Unique per call: a fixed name would collide across concurrent applies and leave a
    // crashed run's initramfs artifacts in the next run's way.
    static APPLY_COUNT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let call = APPLY_COUNT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let work_dir = std::env::temp_dir().join(format!("bootshiftd-{}-{call}", std::process::id()));
    // Create it private (0o700) and fail the call if we cannot: the work dir holds the recovery
    // initramfs we are about to install, so a world-readable or attacker-pre-created directory in
    // a shared /tmp must never be silently reused. `recursive(false)` is deliberate: it errors
    // (AlreadyExists) instead of adopting a pre-existing directory with someone else's mode, which
    // `recursive(true)` would do — the parent temp dir already exists, and the pid+counter name
    // makes a legitimate collision practically impossible.
    {
        use std::os::unix::fs::DirBuilderExt;
        std::fs::DirBuilder::new()
            .recursive(false)
            .mode(0o700)
            .create(&work_dir)
            .map_err(|e| {
                ServiceError::Refused(vec![bootshift_core::refusal::Refusal::new(
                    format!("could not create a private work directory: {e}"),
                    "check permissions on the system temp directory and retry",
                )])
            })?;
    }
    let result = service::apply(
        &SystemRunner,
        &SystemPaths::default(),
        &inspected,
        &plan,
        recovery_binary,
        &work_dir,
    );
    let _ = std::fs::remove_dir_all(&work_dir);
    result.map(|_| responses::Applied {
        restart_to_begin: true,
    })
}

/// Runs slow work (tools, disk reads) off the D-Bus executor, so other calls keep flowing.
async fn run_blocking(
    work: impl FnOnce() -> Result<String, String> + Send + 'static,
) -> fdo::Result<String> {
    blocking::unblock(work).await.map_err(fdo::Error::Failed)
}

fn main() -> anyhow::Result<()> {
    let helper = Helper {
        recovery_binary: PathBuf::from(RECOVERY_BINARY),
    };
    zbus::block_on(async {
        let _connection = zbus::connection::Builder::system()
            .name(BUS_NAME)
            .serve_at(OBJECT_PATH, helper)
            .build()
            .await?;
        std::future::pending::<()>().await;
        Ok::<(), anyhow::Error>(())
    })
}
