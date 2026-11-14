use crate::storage::invalid;
use motor_protocol::{TerminalMemory, TerminalRecord, MAX_FRAME_BYTES};
use serde::Serialize;
use std::io::{self, Write};

// Reserve half the frame for the envelope, execution facts and other metadata.
// A 4096-byte path can need 4096 * 6 + 3 JSON bytes (\u00XX escaping,
// quotes and a comma). Counting encoded bytes, rather than paths, admits many
// ordinary short paths without assuming that every path has the worst size.
const WORKSPACE_CHANGES_BYTES: usize = MAX_FRAME_BYTES / 2;
const TERMINAL_BYTES: usize = MAX_FRAME_BYTES - MAX_FRAME_BYTES / 64;

#[derive(Default)]
struct ByteCount(usize);

impl Write for ByteCount {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0 += bytes.len();
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn encoded_len(value: &impl Serialize) -> io::Result<usize> {
    let mut count = ByteCount::default();
    serde_json::to_writer(&mut count, value).map_err(invalid)?;
    Ok(count.0)
}

fn terminal_len(terminal: &TerminalRecord) -> io::Result<usize> {
    #[derive(Serialize)]
    struct Envelope<'a> {
        #[serde(rename = "type")]
        kind: &'static str,
        #[serde(flatten)]
        terminal: &'a TerminalRecord,
        #[serde(flatten)]
        memory: TerminalMemory,
    }
    // Budget for the widest possible guest-to-host accounting, even when it is
    // still pending at persistence time. Keep another 1/64 frame (16 KiB) free
    // so small envelope additions cannot strand an already stored terminal.
    encoded_len(&Envelope {
        kind: "terminal",
        terminal,
        memory: TerminalMemory {
            mem_peak_mib: Some(u64::MAX),
            mem_working_peak_mib: Some(u64::MAX),
            mem_over_mib_ms: Some(u64::MAX),
        },
    })
}

/// Apply the same limits before storage and transmission, including replay of
/// records written by older runners. Execution facts and accounting are kept.
pub(crate) fn bound(terminal: &mut TerminalRecord) -> io::Result<()> {
    if let Some(changes) = &terminal.workspace_changes {
        let bytes = encoded_len(changes)?;
        if bytes > WORKSPACE_CHANGES_BYTES {
            tracing::warn!(job_id = %terminal.job_id, count = changes.len(), bytes,
                bound = WORKSPACE_CHANGES_BYTES, "workspace_changes unavailable: encoded list exceeds terminal budget");
            terminal.workspace_changes = None;
        }
    }

    // These reports are optional, not partial reports: dropping them preserves
    // the distinction between unavailable metadata and an empty/unchanged tree.
    // Refusal details go last because their producer already caps them at 1 KiB.
    // The runner does not produce pipestatus (pipeline exit codes) or hosted
    // metadata (placement and billing); neither exit status nor billing may be
    // sacrificed to retain optional diagnostics.
    let mut bytes = terminal_len(terminal)?;
    macro_rules! drop_if_needed {
        ($field:ident) => {
            if bytes > TERMINAL_BYTES && terminal.$field.is_some() {
                terminal.$field = None;
                tracing::warn!(job_id = %terminal.job_id, field = stringify!($field),
                    bytes, bound = TERMINAL_BYTES, "dropped optional terminal field to fit frame");
                bytes = terminal_len(terminal)?;
            }
        };
    }
    drop_if_needed!(workspace_changes);
    drop_if_needed!(untracked_files);
    drop_if_needed!(ignored_writes);
    drop_if_needed!(git_state_changed);
    drop_if_needed!(tree_hash);
    drop_if_needed!(oom_kill);
    drop_if_needed!(refusal_detail);
    if bytes > TERMINAL_BYTES {
        return Err(invalid("terminal execution facts exceed frame budget"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use motor_protocol::*;

    #[test]
    fn normal_terminal_is_byte_identical_and_keeps_many_short_paths() {
        let mut terminal = TerminalRecord::new(Uuid::now_v7(), Outcome::Exit { code: 7 }, 42, 3, 8)
            .with_ran(Ran::Remote)
            .with_execution_ended()
            .with_pipestatus(vec![0, 7])
            .with_workspace_changes((0..100).map(|i| format!("src/{i}.rs")).collect())
            .with_tree_hash("a".repeat(64))
            .with_refusal_detail("retained exit status")
            .with_untracked_files(UntrackedFiles::new(vec!["new.rs".into()]))
            .with_ignored_writes(IgnoredWrites::new(1).with_sample_paths(vec!["cache/out".into()]))
            .with_memory_stated_mib(4096)
            .with_last_seq(5);
        let before = terminal.clone();
        let mut original = Vec::new();
        write_frame(&mut original, &Frame::Terminal(before)).unwrap();
        bound(&mut terminal).unwrap();
        let mut after = Vec::new();
        write_frame(&mut after, &Frame::Terminal(terminal)).unwrap();
        assert_eq!(after, original);
    }

    #[test]
    fn workspace_budget_counts_json_escaping_and_keeps_exact_boundary() {
        let mut terminal = TerminalRecord::new(Uuid::now_v7(), Outcome::Exit { code: 0 }, 0, 0, 0)
            .with_workspace_changes(vec!["a".repeat(WORKSPACE_CHANGES_BYTES - 4)]);
        bound(&mut terminal).unwrap();
        assert!(terminal.workspace_changes.is_some());
        terminal.workspace_changes.as_mut().unwrap()[0].push('a');
        bound(&mut terminal).unwrap();
        assert!(terminal.workspace_changes.is_none());

        terminal.workspace_changes = Some(vec!["\u{0001}".repeat(WORKSPACE_CHANGES_BYTES / 6)]);
        bound(&mut terminal).unwrap();
        assert!(terminal.workspace_changes.is_none());
    }

    #[test]
    fn backstop_drops_in_order_and_keeps_bounded_refusal_detail() {
        let mut terminal = TerminalRecord::new(Uuid::now_v7(), Outcome::Exit { code: 1 }, 1, 2, 3)
            .with_workspace_changes(vec!["changed.rs".into()])
            .with_untracked_files(UntrackedFiles::new(vec![
                "u".repeat(MAX_FRAME_BYTES * 3 / 5)
            ]))
            .with_ignored_writes(
                IgnoredWrites::new(7).with_sample_paths(vec!["i".repeat(MAX_FRAME_BYTES * 9 / 20)]),
            )
            .with_refusal_detail("d".repeat(REFUSAL_DETAIL_MAX_BYTES));
        let ignored = terminal.ignored_writes.clone();
        let detail = terminal.refusal_detail.clone();
        bound(&mut terminal).unwrap();
        assert!(terminal.workspace_changes.is_none());
        assert!(terminal.untracked_files.is_none());
        assert_eq!(terminal.ignored_writes, ignored);
        assert_eq!(terminal.refusal_detail, detail);
    }
}
