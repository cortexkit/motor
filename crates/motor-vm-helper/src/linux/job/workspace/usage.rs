//! FIEMAP measures host allocation without reading a guest filesystem. st_blocks
//! alone is insufficient: a reflink reports allocated but still-shared blocks.
use super::*;

const LAST: u32 = 0x1;
const SHARED: u32 = 0x2000;
const UNWRITTEN: u32 = 0x800;
const BATCH: usize = 256;

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct Extent {
    logical: u64,
    physical: u64,
    length: u64,
    reserved64: [u64; 2],
    flags: u32,
    reserved: [u32; 3],
}
#[repr(C)]
struct Map {
    start: u64,
    length: u64,
    flags: u32,
    mapped: u32,
    count: u32,
    reserved: u32,
    extents: [Extent; BATCH],
}

pub(crate) fn exclusive_bytes(file: &File) -> Result<u64> {
    let mut start = 0;
    let mut exclusive = 0u64;
    loop {
        let mut map = Map {
            start,
            length: u64::MAX - start,
            // Flush delayed allocation before interpreting physical extents.
            flags: 1,
            mapped: 0,
            count: BATCH as u32,
            reserved: 0,
            extents: [Extent::default(); BATCH],
        };
        if unsafe { libc::ioctl(file.as_raw_fd(), 0xC020660B as libc::c_ulong, &mut map) } != 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        if map.mapped as usize > BATCH {
            return Err(Error("invalid FIEMAP count".into()));
        }
        if map.mapped == 0 {
            return Ok(exclusive);
        }
        for extent in &map.extents[..map.mapped as usize] {
            // Unknown, delayed, encoded, inline or merged extents are not proof
            // of exclusive allocation. An unsupported probe is absent, not 0.
            if extent.flags & !(LAST | SHARED | UNWRITTEN) != 0
                || extent.length == 0
                || extent.logical < start
            {
                return Err(Error("unsupported FIEMAP extent".into()));
            }
            start = extent
                .logical
                .checked_add(extent.length)
                .ok_or_else(|| Error("FIEMAP overflow".into()))?;
            if extent.flags & SHARED == 0 {
                exclusive = exclusive
                    .checked_add(extent.length)
                    .ok_or_else(|| Error("FIEMAP overflow".into()))?;
            }
        }
        if map.extents[map.mapped as usize - 1].flags & LAST != 0 {
            return Ok(exclusive);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Seek, SeekFrom};

    #[test]
    fn sparse_and_shared_reflink_extents_are_not_exclusive_until_overwritten() {
        let base = super::super::super::tests::scratch();
        std::fs::create_dir_all(&base).unwrap();
        let mut source = std::fs::OpenOptions::new()
            .create_new(true)
            .read(true)
            .write(true)
            .open(base.join("base"))
            .unwrap();
        source.set_len(16 * 1024 * 1024).unwrap();
        assert_eq!(exclusive_bytes(&source).unwrap(), 0);
        source.write_all(&vec![1; 1024 * 1024]).unwrap();
        source.sync_all().unwrap();
        assert_eq!(exclusive_bytes(&source).unwrap(), 1024 * 1024);
        let clone = File::create(base.join("clone")).unwrap();
        fs::reflink(&source, &clone).unwrap();
        assert_eq!(exclusive_bytes(&source).unwrap(), 0);
        assert_eq!(exclusive_bytes(&clone).unwrap(), 0);
        let mut clone = clone;
        clone.seek(SeekFrom::Start(0)).unwrap();
        clone.write_all(&vec![2; 1024 * 1024]).unwrap();
        clone.sync_all().unwrap();
        assert_eq!(exclusive_bytes(&clone).unwrap(), 1024 * 1024);
        assert_eq!(exclusive_bytes(&source).unwrap(), 1024 * 1024);
        std::fs::remove_dir_all(base).unwrap();
    }
}
