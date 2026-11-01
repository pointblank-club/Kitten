//! Asking polkit whether the caller may do something. The GUI runs as the user; only this
//! service runs as root, and it checks every call.

use std::collections::HashMap;

use zbus::fdo;
use zbus::message::Header;
use zbus_polkit::policykit1::{AuthorityProxy, CheckAuthorizationFlags, Subject};

/// Reading disk layouts and running the checks. Allowed for the active local user.
pub const INSPECT: &str = "org.bootshift.inspect";
/// Preparing a change. Always asks for an administrator password, never remembered.
pub const APPLY: &str = "org.bootshift.apply";

pub async fn require(
    connection: &zbus::Connection,
    header: &Header<'_>,
    action: &str,
) -> fdo::Result<()> {
    // Demo-only escape hatch (set by `cargo xtask demo-gui`): the GUI reaches the helper over a
    // forwarded bus with no polkit agent to prompt, so skip the authorization check. This waives
    // *who may call*, never any data-safety check. Compile-gated to debug builds via
    // `cfg!(debug_assertions)`, so it is dead code in a release/packaged daemon and cannot bypass
    // authorization there even if the variable is set.
    if cfg!(debug_assertions) && std::env::var_os("BOOTSHIFT_SKIP_POLKIT").is_some() {
        eprintln!(
            "bootshiftd: BOOTSHIFT_SKIP_POLKIT is set, not checking authorization for {action}"
        );
        return Ok(());
    }
    let subject = Subject::new_for_message_header(header)
        .map_err(|error| fdo::Error::AccessDenied(error.to_string()))?;
    let authority = AuthorityProxy::new(connection).await?;
    let result = authority
        .check_authorization(
            &subject,
            action,
            &HashMap::new(),
            CheckAuthorizationFlags::ALLOW_USER_INTERACTION,
            "",
        )
        .await?;
    if result.is_authorized {
        Ok(())
    } else {
        Err(fdo::Error::AccessDenied(format!(
            "this user isn't allowed to {action}"
        )))
    }
}
