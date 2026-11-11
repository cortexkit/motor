//! Best-effort telemetry from the compiler servers private to one job unit.

use motor_protocol::{RemoteCacheTotals, Uuid};
use serde_json::Value;
use std::path::Path;

/// Read only bounded, no-follow output files. Missing or incomplete statistics
/// (for example after a killed shell) must not change the job's outcome.
pub(crate) fn read(
    state: &Path,
    id: Uuid,
    puts: super::gateway::JobPutTotals,
) -> Option<RemoteCacheTotals> {
    let sccache = crate::writer::fill::statistics(state, id)
        .ok()
        .and_then(|value| sccache(&value));
    let ccache = crate::writer::fill::ccache_statistics(state, id)
        .ok()
        .and_then(|stats| {
            Some(RemoteCacheTotals {
                hits: *stats.get(super::ccache::REMOTE_HIT)?,
                misses: *stats.get("remote_storage_miss")?,
                errors: stats[super::ccache::REMOTE_ERROR]
                    .saturating_add(stats[super::ccache::REMOTE_TIMEOUT]),
                ..Default::default()
            })
        });
    let mut totals = match (sccache, ccache) {
        (Some(a), Some(b)) => RemoteCacheTotals {
            hits: a.hits.saturating_add(b.hits),
            misses: a.misses.saturating_add(b.misses),
            errors: a.errors.saturating_add(b.errors),
            ..Default::default()
        },
        (a, b) => a.or(b).unwrap_or_default(),
    };
    totals.puts_accepted = puts.accepted;
    totals.puts_duplicate = puts.duplicate;
    totals.puts_failed = puts.failed;
    totals.puts_refused = puts.refused;
    totals.put_bytes_accepted = puts.accepted_bytes;
    (totals != RemoteCacheTotals::default()).then_some(totals)
}

fn sccache(value: &Value) -> Option<RemoteCacheTotals> {
    let stats = value.get("stats").unwrap_or(value);
    Some(RemoteCacheTotals {
        hits: count(stats.get("cache_hits")?)?,
        misses: count(stats.get("cache_misses")?)?,
        errors: count(stats.get("cache_errors")?)?,
        ..Default::default()
    })
}

fn count(value: &Value) -> Option<u64> {
    value.as_u64().or_else(|| {
        // adv_counts is a more detailed view of the same counts, not extra work.
        value
            .get("counts")?
            .as_object()?
            .values()
            .try_fold(0u64, |sum, n| Some(sum.saturating_add(n.as_u64()?)))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn put_only_totals_survive_missing_or_zero_compiler_statistics() {
        use super::super::gateway::JobPutTotals;
        let state = cortexkit_test_support::ScratchDir::new("put-totals");
        let id = Uuid::now_v7();
        let output = state.join("sandboxes").join(id.to_string()).join("output");
        std::fs::create_dir_all(&output).unwrap();
        for stats in [
            None,
            Some("invalid"),
            Some(r#"{"cache_hits":0,"cache_misses":0,"cache_errors":0}"#),
        ] {
            if let Some(stats) = stats {
                std::fs::write(output.join("cache-stats"), stats).unwrap();
            }
            for (puts, expected) in [
                (
                    JobPutTotals {
                        accepted: 1,
                        ..Default::default()
                    },
                    RemoteCacheTotals {
                        puts_accepted: 1,
                        ..Default::default()
                    },
                ),
                (
                    JobPutTotals {
                        duplicate: 2,
                        ..Default::default()
                    },
                    RemoteCacheTotals {
                        puts_duplicate: 2,
                        ..Default::default()
                    },
                ),
                (
                    JobPutTotals {
                        failed: 3,
                        ..Default::default()
                    },
                    RemoteCacheTotals {
                        puts_failed: 3,
                        ..Default::default()
                    },
                ),
                (
                    JobPutTotals {
                        refused: 4,
                        ..Default::default()
                    },
                    RemoteCacheTotals {
                        puts_refused: 4,
                        ..Default::default()
                    },
                ),
                (
                    JobPutTotals {
                        accepted_bytes: 5,
                        ..Default::default()
                    },
                    RemoteCacheTotals {
                        put_bytes_accepted: 5,
                        ..Default::default()
                    },
                ),
            ] {
                assert_eq!(read(&state, id, puts), Some(expected));
            }
        }
    }

    #[test]
    fn sccache_counts_languages_once_and_accepts_numeric_counters() {
        assert_eq!(
            sccache(&json!({"stats": {
                "cache_hits": {"counts": {"Rust": 7, "C/C++": 2}, "adv_counts": {"rust": 7}},
                "cache_misses": {"counts": {"Rust": 3}},
                "cache_errors": {"counts": {"Rust": 1}}
            }})),
            Some(RemoteCacheTotals {
                hits: 9,
                misses: 3,
                errors: 1,
                ..Default::default()
            })
        );
        assert_eq!(
            sccache(&json!({"cache_hits": 4, "cache_misses": 2, "cache_errors": 0})),
            Some(RemoteCacheTotals {
                hits: 4,
                misses: 2,
                errors: 0,
                ..Default::default()
            })
        );
    }

    #[test]
    fn invalid_counters_are_unknown_and_large_counters_do_not_overflow() {
        for hits in [json!(-1), json!({"counts": {"Rust": "7"}}), json!({})] {
            assert_eq!(
                sccache(&json!({"cache_hits": hits, "cache_misses": 2, "cache_errors": 0})),
                None
            );
        }
        assert_eq!(sccache(&json!({"cache_misses": 2})), None);
        assert_eq!(
            count(&json!({"counts": {"Rust": u64::MAX, "C/C++": 1}})),
            Some(u64::MAX)
        );
    }

    #[test]
    fn output_is_job_private_bounded_and_combines_only_valid_tool_statistics() {
        use std::fs;
        let state =
            cortexkit_test_support::scratch_root().join(format!("cache-totals-{}", Uuid::now_v7()));
        let id = Uuid::now_v7();
        let output = state.join("sandboxes").join(id.to_string()).join("output");
        fs::create_dir_all(&output).unwrap();
        assert_eq!(read(&state, id, Default::default()), None);
        let sccache = output.join("cache-stats");
        fs::write(
            &sccache,
            r#"{"stats":{"cache_hits":7,"cache_misses":3,"cache_errors":1}}"#,
        )
        .unwrap();
        assert_eq!(
            read(&state, id, Default::default()),
            Some(RemoteCacheTotals {
                hits: 7,
                misses: 3,
                errors: 1,
                ..Default::default()
            })
        );
        assert_eq!(read(&state, Uuid::now_v7(), Default::default()), None);
        let ccache = output.join("ccache-stats");
        fs::write(&ccache, "remote_storage_hit\t2\nremote_storage_miss\t4\nremote_storage_error\t1\nremote_storage_timeout\t2\n").unwrap();
        assert_eq!(
            read(&state, id, Default::default()),
            Some(RemoteCacheTotals {
                hits: 9,
                misses: 7,
                errors: 4,
                ..Default::default()
            })
        );
        fs::write(&sccache, b"not JSON").unwrap();
        assert_eq!(
            read(&state, id, Default::default()),
            Some(RemoteCacheTotals {
                hits: 2,
                misses: 4,
                errors: 3,
                ..Default::default()
            })
        );
        fs::write(&ccache, "remote_storage_hit\t2\n").unwrap();
        assert_eq!(read(&state, id, Default::default()), None);
        let valid = br#"{"cache_hits":7,"cache_misses":3,"cache_errors":1}"#;
        let mut oversized = valid.to_vec();
        oversized.resize(64 * 1024 + 1, b' ');
        fs::write(&sccache, oversized).unwrap();
        assert_eq!(read(&state, id, Default::default()), None);
        fs::remove_file(&sccache).unwrap();
        fs::write(&ccache, valid).unwrap();
        std::os::unix::fs::symlink(&ccache, &sccache).unwrap();
        assert_eq!(read(&state, id, Default::default()), None);
        fs::remove_file(&sccache).unwrap();
        fs::write(
            &sccache,
            r#"{"cache_hits":0,"cache_misses":0,"cache_errors":0}"#,
        )
        .unwrap();
        assert_eq!(read(&state, id, Default::default()), None);
        fs::remove_dir_all(state).unwrap();
    }
}
