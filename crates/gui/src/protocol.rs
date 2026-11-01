//! The JSON `bootshiftd` speaks. Layouts, targets, operations and refusals are `core`'s own
//! types, so the GUI reads exactly what the helper writes. Positions and lengths are sectors.

use bootshift_core::layout::{Layout, Target};
use bootshift_core::plan::Operation;
use bootshift_core::refusal::Refusal;
use serde::Deserialize;

#[derive(Clone, Debug, PartialEq, Deserialize)]
#[serde(tag = "result", rename_all = "snake_case")]
pub enum Reply<T> {
    Ok { value: T },
    Refused { refusals: Vec<Refusal> },
}

#[derive(Clone, Debug, PartialEq, Deserialize)]
pub struct DiskSummary {
    pub path: String,
    pub size: u64,
    pub transport: Option<String>,
    pub removable: bool,
    pub partition_table: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Deserialize)]
pub struct Probed {
    pub disk: String,
    pub size: u64,
    pub layout: Layout,
    pub target: Target,
}

#[derive(Clone, Debug, Default, PartialEq, Deserialize)]
pub struct Planned {
    pub operations: Vec<Operation>,
    pub copy_bytes: u64,
    /// Steps this build can't do safely yet. The plan can't proceed while any remain.
    pub unavailable: Vec<Refusal>,
}

/// One named preflight check, as the checks screen lists it.
#[derive(Clone, Debug, PartialEq, Deserialize)]
pub struct CheckResult {
    pub name: String,
    pub detail: String,
    pub passed: bool,
}

#[derive(Clone, Debug, PartialEq, Deserialize)]
pub struct Applied {
    pub restart_to_begin: bool,
}

/// The outcome of the last run, read back after the restart that applied it. The helper reads
/// this from the journal the recovery system left on the EFI partition, so reopening Kitten
/// after the reboot shows the finished result instead of the goal launcher.
#[derive(Clone, Debug, PartialEq, Deserialize)]
pub struct FinishedRun {
    pub bytes: u64,
    pub chunks: u64,
    pub power_cuts: u32,
    pub seconds: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_refused_reply_carries_every_refusal_with_its_fix() {
        let json = r#"{"result":"refused","refusals":[{"reason":"Windows is hibernated",
            "fix":"Turn off Fast Startup and shut Windows down fully"}]}"#;
        let reply: Reply<Planned> = serde_json::from_str(json).unwrap();
        assert_eq!(
            reply,
            Reply::Refused {
                refusals: vec![Refusal::new(
                    "Windows is hibernated",
                    "Turn off Fast Startup and shut Windows down fully"
                )]
            }
        );
    }

    #[test]
    fn a_plan_reply_reads_operations_in_cores_format() {
        let json = r#"{"result":"ok","value":{"operations":[{"Shrink":{
            "partition":"00000003-0000-4000-8000-000000000003","len":2048}}],
            "copy_bytes":0,"unavailable":[]}}"#;
        let reply: Reply<Planned> = serde_json::from_str(json).unwrap();
        let Reply::Ok { value } = reply else {
            panic!("expected a plan");
        };
        assert!(matches!(value.operations[0], Operation::Shrink { .. }));
    }

    #[test]
    fn a_check_reply_without_rows_still_reads() {
        let reply: Reply<Option<Vec<CheckResult>>> =
            serde_json::from_str(r#"{"result":"ok","value":null}"#).unwrap();
        assert_eq!(reply, Reply::Ok { value: None });
    }
}
