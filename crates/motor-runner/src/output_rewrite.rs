//! Streaming path substitution with source-byte accounting for durable replay.

use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Deserialize, Serialize, Eq, PartialEq)]
pub(crate) struct Rewrite {
    pub(crate) fixed: String,
    pub(crate) caller: String,
}

#[cfg(target_os = "linux")]
pub(crate) fn for_job(config: &crate::Config, request: &motor_protocol::Run) -> Vec<Rewrite> {
    if config.vm_instance() {
        return Vec::new();
    }
    crate::caller_path::fixed_paths(config, request)
        .ok()
        .flatten()
        .map(|fixed| {
            std::iter::once(Rewrite {
                fixed: fixed.checkout.to_string_lossy().into_owned(),
                caller: crate::caller_path::caller_checkout(request)
                    .to_string_lossy()
                    .into_owned(),
            })
            .chain(
                fixed
                    .siblings
                    .iter()
                    .zip(&request.siblings)
                    .map(|(fixed, sibling)| Rewrite {
                        fixed: fixed.to_string_lossy().into_owned(),
                        caller: sibling.canonical_path.clone(),
                    }),
            )
            .collect()
        })
        .unwrap_or_default()
}

pub(crate) struct Page {
    pub(crate) bytes: Vec<u8>,
    pub(crate) source_len: u64,
}

#[derive(Default)]
pub(crate) struct Stream {
    pending: Vec<u8>,
    /// Durable log position read, including bytes waiting for a boundary.
    pub(crate) cursor: u64,
}

impl Stream {
    pub(crate) fn reset(&mut self, committed: u64) {
        self.pending.clear();
        self.cursor = committed;
    }

    pub(crate) fn pending_len(&self) -> usize {
        self.pending.len()
    }

    pub(crate) fn push(
        &mut self,
        rewrites: &[Rewrite],
        bytes: &[u8],
        eof: bool,
        page_size: usize,
    ) -> Vec<Page> {
        let mut rules: Vec<_> = rewrites.iter().filter(|r| !r.fixed.is_empty()).collect();
        rules.sort_by_key(|r| std::cmp::Reverse(r.fixed.len()));
        let mut source = std::mem::take(&mut self.pending);
        source.extend_from_slice(bytes);
        let mut pages = Vec::new();
        let mut page = Page {
            bytes: Vec::new(),
            source_len: 0,
        };
        let mut at = 0;
        while at < source.len() {
            let rest = &source[at..];
            let mut replacement = None;
            let mut incomplete = false;
            for rule in &rules {
                let fixed = rule.fixed.as_bytes();
                if !eof && fixed.starts_with(rest) {
                    incomplete = true;
                    break;
                }
                if rest.starts_with(fixed)
                    && (rest.len() == fixed.len() || boundary(rest[fixed.len()]))
                {
                    replacement = Some(*rule);
                    break;
                }
            }
            if incomplete {
                break;
            }
            let (emitted, consumed) = match replacement {
                Some(rule) => (rule.caller.as_bytes(), rule.fixed.len()),
                None => (&source[at..at + 1], 1),
            };
            // A replacement is indivisible: its source offset is committed only
            // when all its output bytes are in the same persisted page.
            if !page.bytes.is_empty() && page.bytes.len() + emitted.len() > page_size {
                pages.push(page);
                page = Page {
                    bytes: Vec::new(),
                    source_len: 0,
                };
            }
            page.bytes.extend_from_slice(emitted);
            page.source_len += consumed as u64;
            at += consumed;
        }
        if page.source_len != 0 {
            pages.push(page);
        }
        self.pending.extend_from_slice(&source[at..]);
        debug_assert!(self.pending.len() <= rules.first().map_or(0, |r| r.fixed.len()));
        pages
    }
}

fn boundary(byte: u8) -> bool {
    matches!(
        byte,
        b'/' | b':'
            | b' '
            | b'\t'
            | b'\r'
            | b'\n'
            | b'"'
            | b'\''
            | b'`'
            | b')'
            | b']'
            | b'}'
            | b'>'
            | b','
            | b';'
            | 0x1b
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rule(fixed: &str, caller: &str) -> Rewrite {
        Rewrite {
            fixed: fixed.into(),
            caller: caller.into(),
        }
    }

    fn collect(pages: Vec<Page>) -> Vec<u8> {
        pages.into_iter().flat_map(|page| page.bytes).collect()
    }

    #[test]
    fn longest_match_across_every_write_split_and_page_boundary() {
        let rules = [
            rule("/w/repo", "/caller"),
            rule("/w/repo/sub", "/longer/caller/sub"),
        ];
        let source = b"/w/repo/sub:12 /w/repo/)";
        let expected = b"/longer/caller/sub:12 /caller/)";
        for split in 0..=source.len() {
            let mut stream = Stream::default();
            let mut output = collect(stream.push(&rules, &source[..split], false, 1024));
            assert!(stream.pending_len() <= rules[1].fixed.len() + 1);
            output.extend(collect(stream.push(&rules, &source[split..], true, 1024)));
            assert_eq!(output, expected, "split {split}");
        }
        let page_size = 1024 * 1024;
        let mut source = vec![b'x'; page_size - 3];
        source.extend_from_slice(b"/w/repo/sub:12");
        let pages = Stream::default().push(&rules, &source, true, page_size);
        assert_eq!(pages.len(), 2);
        assert_eq!(pages[0].bytes.len(), page_size - 3);
        assert_eq!(pages[0].source_len, (page_size - 3) as u64);
        assert_eq!(pages[1].bytes, b"/longer/caller/sub:12");
        assert_eq!(pages[1].source_len, b"/w/repo/sub:12".len() as u64);
    }

    #[test]
    fn only_listed_boundary_bytes_or_eof_allow_rewriting() {
        let rules = [rule("/w/repo", "/caller")];
        for suffix in ["sitory", "+old", "@debug", "~", "-", ".", "é"] {
            let source = format!("/w/repo{suffix}");
            let output = collect(Stream::default().push(&rules, source.as_bytes(), true, 1024));
            assert_eq!(output, source.as_bytes(), "suffix {suffix}");
        }
        for suffix in [
            "/", ":12", " ", "\t", "\r", "\n", "\"", "'", "`", ")", "]", "}", ">", ",", ";",
            "\x1b", "",
        ] {
            let source = format!("/w/repo{suffix}");
            let output = collect(Stream::default().push(&rules, source.as_bytes(), true, 1024));
            assert_eq!(
                output,
                format!("/caller{suffix}").as_bytes(),
                "suffix {suffix:?}"
            );
        }
    }

    #[test]
    fn eof_resolves_complete_and_partial_paths_without_rescanning_insertions() {
        let rules = [rule("/w/repo", "/w/repo/sub"), rule("/w/repo/sub", "/s")];
        for source in [b"/w/repo".as_slice(), b"/w/repo/sub", b"/w/rep"] {
            for split in 0..=source.len() {
                let mut stream = Stream::default();
                let mut output = collect(stream.push(&rules, &source[..split], false, 1024));
                output.extend(collect(stream.push(&rules, &source[split..], false, 1024)));
                output.extend(collect(stream.push(&rules, &[], true, 1024)));
                let expected = match source {
                    b"/w/repo" => b"/w/repo/sub".as_slice(),
                    b"/w/repo/sub" => b"/s",
                    _ => source,
                };
                assert_eq!(output, expected, "split {split}");
                assert_eq!(stream.pending_len(), 0);
            }
        }
        let binary = b"\0\xffuntouched\x1b\xfe/w/rep\0";
        assert_eq!(
            collect(Stream::default().push(&rules, binary, true, 1024)),
            binary
        );
    }
}
