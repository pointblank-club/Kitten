//! How the recovery system learns which disk to work on and where the journal is: two kernel
//! command line options, so it can find the journal without mounting the ESP.
//!
//! `bootshift.disk=<disk GUID> bootshift.journal=<offset>+<len>,<offset>+<len>,…`
//!
//! Journal pieces are byte offsets on the whole disk, in file order.

use bootshift_core::layout::Guid;
use bootshift_disk::FileExtent;

const DISK_KEY: &str = "bootshift.disk=";
const JOURNAL_KEY: &str = "bootshift.journal=";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RecoveryArgs {
    pub disk_guid: Guid,
    pub journal: Vec<FileExtent>,
}

#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum CmdlineError {
    #[error("the kernel command line has no {0}")]
    Missing(&'static str),
    #[error("the kernel command line option {0} is malformed")]
    Malformed(String),
}

impl RecoveryArgs {
    pub fn to_kernel_args(&self) -> String {
        let journal: Vec<String> = self
            .journal
            .iter()
            .map(|extent| format!("{}+{}", extent.device_offset, extent.len))
            .collect();
        format!(
            "{DISK_KEY}{} {JOURNAL_KEY}{}",
            self.disk_guid,
            journal.join(",")
        )
    }

    pub fn from_cmdline(cmdline: &str) -> Result<RecoveryArgs, CmdlineError> {
        let value = |key: &'static str| {
            cmdline
                .split_whitespace()
                .find_map(|word| word.strip_prefix(key))
                .ok_or(CmdlineError::Missing(key))
        };
        let disk = value(DISK_KEY)?;
        let disk_guid = disk
            .parse()
            .map_err(|_| CmdlineError::Malformed(format!("{DISK_KEY}{disk}")))?;
        let journal_text = value(JOURNAL_KEY)?;
        let malformed = || CmdlineError::Malformed(format!("{JOURNAL_KEY}{journal_text}"));

        let mut journal = Vec::new();
        let mut file_offset = 0;
        for piece in journal_text.split(',') {
            let (offset, len) = piece.split_once('+').ok_or_else(malformed)?;
            let device_offset = offset.parse().map_err(|_| malformed())?;
            let len: u64 = len.parse().map_err(|_| malformed())?;
            journal.push(FileExtent {
                file_offset,
                device_offset,
                len,
            });
            // This parses untrusted recovery args and runs as PID 1 in the recovery system, where a
            // panic is a kernel panic. Refuse an overflowing cumulative offset rather than relying on
            // release overflow-checks to abort.
            file_offset = file_offset.checked_add(len).ok_or_else(malformed)?;
        }
        Ok(RecoveryArgs { disk_guid, journal })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args() -> RecoveryArgs {
        RecoveryArgs {
            disk_guid: Guid([0xD1; 16]),
            journal: vec![
                FileExtent {
                    file_offset: 0,
                    device_offset: 1_048_576,
                    len: 2_097_152,
                },
                FileExtent {
                    file_offset: 2_097_152,
                    device_offset: 9_437_184,
                    len: 2_097_152,
                },
            ],
        }
    }

    #[test]
    fn round_trips_through_a_full_kernel_command_line() {
        let cmdline = format!(
            "BOOT_IMAGE=/vmlinuz ro quiet {} splash",
            args().to_kernel_args()
        );
        assert_eq!(RecoveryArgs::from_cmdline(&cmdline), Ok(args()));
    }

    #[test]
    fn missing_options_are_named() {
        assert_eq!(
            RecoveryArgs::from_cmdline("ro quiet"),
            Err(CmdlineError::Missing("bootshift.disk="))
        );
    }

    #[test]
    fn malformed_journal_pieces_are_refused() {
        let cmdline = format!("bootshift.disk={} bootshift.journal=12+x", Guid([1; 16]));
        assert!(matches!(
            RecoveryArgs::from_cmdline(&cmdline),
            Err(CmdlineError::Malformed(_))
        ));
    }
}
