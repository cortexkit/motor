//! Validated Firecracker UFFD layouts. Only 2 MiB missing-page restores are supported.
use serde::Deserialize;
use std::io;

pub const PAGE_SIZE: u64 = 2 * 1024 * 1024;
pub const MAX_HANDSHAKE: usize = 64 * 1024;

pub fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Region {
    pub base_host_virt_addr: u64,
    pub size: u64,
    pub offset: u64,
    pub page_size: u64,
}

#[derive(Debug, PartialEq, Eq)]
pub struct Copy {
    pub destination: u64,
    pub offset: u64,
    pub length: u64,
}

pub struct Layout(Vec<Region>);
impl Layout {
    pub fn parse(bytes: &[u8], memory_size: u64) -> io::Result<Self> {
        if bytes.len() > MAX_HANDSHAKE {
            return Err(invalid("UFFD handshake too large"));
        }
        let regions = serde_json::from_slice(bytes).map_err(|e| invalid(&e.to_string()))?;
        Self::new(regions, memory_size)
    }

    pub fn new(regions: Vec<Region>, memory_size: u64) -> io::Result<Self> {
        if regions.is_empty() || regions.len() > 256 || memory_size == 0 {
            return Err(invalid("invalid region count or memory size"));
        }
        for region in &regions {
            if region.page_size != PAGE_SIZE
                || region.size == 0
                || [region.base_host_virt_addr, region.offset, region.size]
                    .iter()
                    .any(|n| n % PAGE_SIZE != 0)
                || region
                    .base_host_virt_addr
                    .checked_add(region.size)
                    .is_none()
                || region
                    .offset
                    .checked_add(region.size)
                    .is_none_or(|end| end > memory_size)
            {
                return Err(invalid(
                    "unaligned, unsupported or out-of-bounds UFFD region",
                ));
            }
        }
        let mut by_host: Vec<_> = regions.iter().collect();
        by_host.sort_by_key(|r| r.base_host_virt_addr);
        if by_host
            .windows(2)
            .any(|r| r[0].base_host_virt_addr + r[0].size > r[1].base_host_virt_addr)
        {
            return Err(invalid("overlapping host regions"));
        }
        let mut by_file: Vec<_> = regions.iter().collect();
        by_file.sort_by_key(|r| r.offset);
        let mut end = 0;
        for region in by_file {
            if region.offset != end {
                return Err(invalid("overlapping or incomplete snapshot layout"));
            }
            end += region.size;
        }
        if end != memory_size {
            return Err(invalid("layout does not cover memory fd"));
        }
        Ok(Self(regions))
    }

    pub fn locate(&self, address: u64) -> io::Result<Copy> {
        for r in &self.0 {
            if address >= r.base_host_virt_addr && address < r.base_host_virt_addr + r.size {
                let destination = address & !(PAGE_SIZE - 1);
                return Ok(Copy {
                    destination,
                    offset: r.offset + destination - r.base_host_virt_addr,
                    length: PAGE_SIZE,
                });
            }
        }
        Err(invalid("fault outside registered memory"))
    }
}

#[cfg(target_os = "linux")]
pub mod linux;

#[cfg(test)]
mod tests {
    use super::*;
    fn region(base: u64, offset: u64, size: u64) -> Region {
        Region {
            base_host_virt_addr: base,
            offset,
            size,
            page_size: PAGE_SIZE,
        }
    }
    #[test]
    fn faults_copy_exactly_two_mib_at_region_boundaries() {
        let layout = Layout::new(
            vec![
                region(8 * PAGE_SIZE, 0, 2 * PAGE_SIZE),
                region(32 * PAGE_SIZE, 2 * PAGE_SIZE, PAGE_SIZE),
            ],
            3 * PAGE_SIZE,
        )
        .unwrap();
        assert_eq!(
            layout.locate(10 * PAGE_SIZE - 1).unwrap(),
            Copy {
                destination: 9 * PAGE_SIZE,
                offset: PAGE_SIZE,
                length: PAGE_SIZE
            }
        );
        assert_eq!(layout.locate(32 * PAGE_SIZE).unwrap().offset, 2 * PAGE_SIZE);
        assert!(layout.locate(10 * PAGE_SIZE).is_err());
        assert!(layout.locate(8 * PAGE_SIZE - 1).is_err());
    }
    #[test]
    fn handshake_rejects_bounds_overflow_alignment_overlap_and_gaps() {
        for r in [
            region(0, PAGE_SIZE, PAGE_SIZE),
            region(u64::MAX - PAGE_SIZE + 1, 0, PAGE_SIZE),
            region(1, 0, PAGE_SIZE),
            region(0, 0, 1),
        ] {
            assert!(Layout::new(vec![r], PAGE_SIZE).is_err());
        }
        assert!(Layout::new(
            vec![region(0, 0, PAGE_SIZE), region(0, PAGE_SIZE, PAGE_SIZE)],
            2 * PAGE_SIZE
        )
        .is_err());
        assert!(Layout::new(
            vec![region(0, 0, PAGE_SIZE), region(4 * PAGE_SIZE, 0, PAGE_SIZE)],
            2 * PAGE_SIZE
        )
        .is_err());
        assert!(Layout::new(vec![region(0, 0, PAGE_SIZE)], 2 * PAGE_SIZE).is_err());
        let mut r = region(0, 0, PAGE_SIZE);
        r.page_size = 4096;
        assert!(Layout::new(vec![r], PAGE_SIZE).is_err());
        assert!(Layout::parse(&vec![b' '; MAX_HANDSHAKE + 1], PAGE_SIZE).is_err());
        assert!(Layout::parse(br#"[{"size":-1}]"#, PAGE_SIZE).is_err());
    }
}
