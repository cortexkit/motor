//! Bun 1.4.4 uses legacy Wyhash11 for cache keys, not modern wyhash/final4.
//! Algorithm reference: oven-sh/bun bun-v1.4.4 src/wyhash/lib.rs (MIT).

const P: [u64; 5] = [
    0xa0761d6478bd642f,
    0xe7037ed1a0b428db,
    0x8ebc6af09c88c6e3,
    0x589965cc75374cc3,
    0x1d8e4e27c47d124f,
];
fn mum(a: u64, b: u64) -> u64 {
    let product = a as u128 * b as u128;
    product as u64 ^ (product >> 64) as u64
}
fn little(bytes: &[u8]) -> u64 {
    bytes
        .iter()
        .enumerate()
        .fold(0, |n, (i, b)| n | (*b as u64) << (8 * i))
}
fn short(bytes: &[u8]) -> u64 {
    match bytes.len() {
        0 => P[4],
        1 | 2 | 4 => little(bytes),
        8 => (little(&bytes[..4]) << 32) | little(&bytes[4..]),
        n => {
            let head = if n > 4 { 4 } else { 2 };
            (little(&bytes[..head]) << (8 * (n - head))) | short(&bytes[head..])
        }
    }
}
fn mix(bytes: &[u8], seed: u64, offset: usize) -> u64 {
    let head = bytes.len().min(8);
    mum(
        short(&bytes[..head]) ^ seed ^ P[offset],
        short(&bytes[head..]) ^ seed ^ P[offset + 1],
    )
}
pub(super) fn wyhash11(bytes: &[u8]) -> u64 {
    let mut chunks = bytes.chunks_exact(32);
    let mut seed = 0;
    for b in &mut chunks {
        seed = mum(
            little(&b[..8]) ^ seed ^ P[0],
            little(&b[8..16]) ^ seed ^ P[1],
        ) ^ mum(
            little(&b[16..24]) ^ seed ^ P[2],
            little(&b[24..]) ^ seed ^ P[3],
        );
    }
    let tail = chunks.remainder();
    seed = match tail.len() {
        0 => seed,
        1..=16 => mix(tail, seed, 0),
        _ => mix(&tail[..16], seed, 0) ^ mix(&tail[16..], seed, 2),
    };
    mum(seed ^ bytes.len() as u64, P[4])
}

#[cfg(test)]
mod tests {
    #[test]
    fn legacy_hash_matches_independent_bun_144_vectors_at_round_boundaries() {
        // Goldens from the pinned Bun WyhashStateless implementation, not this port.
        for (length, expected) in [
            (0, 0),
            (1, 0x77fbf2f640c3f8da),
            (3, 0x091215339db57eaf),
            (7, 0x86f79d339118912f),
            (8, 0x75fe13f67694746b),
            (16, 0xcb3f4a29a5bd9728),
            (17, 0x5cf4826d91744dfa),
            (23, 0x7402cadd0d36e9ee),
            (24, 0xd2168bf52337292e),
            (31, 0x3ee9a9f997b60f93),
            (32, 0x39e772b36dfce865),
            (33, 0xc22b9dd5d1d1b708),
            (64, 0xbba2784a09ada4f8),
        ] {
            assert_eq!(
                super::wyhash11(&(0..length).collect::<Vec<u8>>()),
                expected,
                "length {length}"
            );
        }
    }
}
