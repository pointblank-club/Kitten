//! Small helpers for reading the text the filesystem tools print.

use std::ops::Range;

use crate::run::{CommandLine, RunError};

/// The value after `name:` on the one line that starts with `name`, trimmed. Refuses when MORE than
/// one line matches: an attacker-controlled string field in the tool output (an ext4 volume label or
/// last-mounted path) can embed a newline to inject a second, fake `Key: value` line, so a
/// security-critical read must treat a duplicate key as tampering rather than silently take the
/// first. `Ok(None)` means the field is absent.
pub(crate) fn field<'a>(text: &'a str, name: &str) -> Result<Option<&'a str>, String> {
    let mut matches = text.lines().filter_map(|line| {
        line.strip_prefix(name)?
            .trim_start()
            .strip_prefix(':')
            .map(str::trim)
    });
    let first = matches.next();
    if first.is_some() && matches.next().is_some() {
        return Err(format!(
            "ambiguous \"{name}\": more than one line matches (possible injected field)"
        ));
    }
    Ok(first)
}

/// The leading number of a field's value, such as `4096` in `Cluster size : 4096 bytes`.
pub(crate) fn number_field(text: &str, name: &str) -> Result<u64, String> {
    field(text, name)?
        .and_then(|value| value.split_whitespace().next())
        .and_then(|number| number.parse().ok())
        .ok_or_else(|| format!("missing or unreadable \"{name}\""))
}

/// Converts a block/extent count into bytes, refusing (rather than wrapping) if the
/// multiplication overflows. The inputs come from tool output describing an untrusted
/// disk, so a corrupt or hostile superblock must produce a clean error, not a wrap or
/// an abort. `overflow-checks` in the release profile would otherwise turn these into a
/// panic; this keeps them a refusal.
pub(crate) fn blocks_to_bytes(blocks: u64, block_size: u64) -> Result<u64, String> {
    blocks
        .checked_mul(block_size)
        .ok_or_else(|| format!("{blocks} blocks of {block_size} bytes overflows a 64-bit size"))
}

/// The parts of `0..total` not covered by `taken`.
pub(crate) fn complement(mut taken: Vec<Range<u64>>, total: u64) -> Vec<Range<u64>> {
    taken.sort_by_key(|r| r.start);
    let mut gaps = Vec::new();
    let mut cursor = 0;
    for range in taken {
        if range.start > cursor {
            gaps.push(cursor..range.start.min(total));
        }
        cursor = cursor.max(range.end);
    }
    if cursor < total {
        gaps.push(cursor..total);
    }
    gaps.retain(|r| r.start < r.end);
    gaps
}

pub(crate) fn unexpected(command: &CommandLine, detail: String) -> RunError {
    RunError::UnexpectedOutput {
        command: command.to_string(),
        detail,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn field_matches_the_whole_name() {
        let text = "Reserved block count: 5\nBlock count:   100\n";
        assert_eq!(field(text, "Block count"), Ok(Some("100")));
    }

    #[test]
    fn number_field_reads_the_leading_number() {
        assert_eq!(
            number_field("Cluster size       : 4096 bytes", "Cluster size"),
            Ok(4096)
        );
        assert!(number_field("Cluster size : big", "Cluster size").is_err());
    }

    #[test]
    fn a_duplicated_key_is_refused_as_injection() {
        // Simulates an ext4 volume label that embedded a newline to inject a fake "Block size:512"
        // line ahead of the real one; a security-critical read must refuse, not take the first.
        let text = "Filesystem volume name: evil\nBlock size:512\nBlock size:              4096\n";
        assert!(field(text, "Block size").is_err());
        assert!(number_field(text, "Block size").is_err());
    }

    #[test]
    fn complement_fills_the_gaps() {
        assert_eq!(complement(vec![5..8, 0..2], 10), vec![2..5, 8..10]);
        assert_eq!(complement(Vec::new(), 4), [0..4]);
        assert!(complement(std::iter::once(0..4).collect(), 4).is_empty());
    }
}
