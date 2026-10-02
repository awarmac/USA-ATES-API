//! A small PMTiles v3 writer (and a reader for tests).
//!
//! PMTiles packs a whole tile pyramid into one file that a browser reads
//! with HTTP range requests, so no tile server is needed. Spec:
//! <https://github.com/protomaps/PMTiles/blob/main/spec/v3/spec.md>.
//!
//! The writer keeps things simple:
//! - The directory and metadata are not compressed (internal compression
//!   "none").
//! - Identical tiles are stored once, and runs of identical consecutive
//!   tiles become one entry.
//! - Only a root directory is written, which holds a few thousand entries.
//!   Larger pyramids need leaf directories (not implemented), and are
//!   refused rather than written wrong.

use std::collections::HashMap;

use ates_core::tiles::tile_id;

use crate::IoError;

const HEADER_LEN: usize = 127;
/// The root directory must end within the first 16 KiB.
const ROOT_LIMIT: usize = 16_384 - HEADER_LEN;

/// Tile content type (PMTiles `TileType`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TileType {
    Png = 2,
    Jpeg = 3,
    Webp = 4,
}

/// File-level information for the header and metadata.
#[derive(Debug, Clone)]
pub struct PmTilesInfo {
    pub tile_type: TileType,
    /// `[west, south, east, north]` in WGS 84 degrees.
    pub bounds: [f64; 4],
    /// Center (lon, lat) and zoom to open at.
    pub center: (f64, f64, u8),
    /// JSON object (TileJSON-style keys plus anything else).
    pub metadata: serde_json::Value,
}

struct Entry {
    tile_id: u64,
    offset: u64,
    length: u64,
    run_length: u64,
}

fn varint(out: &mut Vec<u8>, mut v: u64) {
    while v >= 0x80 {
        out.push((v as u8 & 0x7f) | 0x80);
        v >>= 7;
    }
    out.push(v as u8);
}

fn e7(v: f64) -> [u8; 4] {
    ((v * 1e7).round() as i32).to_le_bytes()
}

/// Write a PMTiles archive. `tiles` are (z, x, y, bytes); empty tiles
/// should simply be left out.
pub fn write_pmtiles(
    tiles: &[(u8, u32, u32, Vec<u8>)],
    info: &PmTilesInfo,
) -> Result<Vec<u8>, IoError> {
    if tiles.is_empty() {
        return Err(IoError::Invalid("PMTiles: no tiles to write".into()));
    }
    let mut order: Vec<(u64, usize)> = tiles
        .iter()
        .enumerate()
        .map(|(i, (z, x, y, _))| (tile_id(*z, *x, *y), i))
        .collect();
    order.sort_unstable();
    if order.windows(2).any(|w| w[0].0 == w[1].0) {
        return Err(IoError::Invalid("PMTiles: duplicate tile".into()));
    }

    // Tile data, in tile-id order, each distinct content stored once.
    let mut data = Vec::new();
    let mut seen: HashMap<&[u8], u64> = HashMap::new();
    let mut entries: Vec<Entry> = Vec::new();
    for &(id, i) in &order {
        let bytes = tiles[i].3.as_slice();
        if bytes.is_empty() {
            return Err(IoError::Invalid("PMTiles: empty tile".into()));
        }
        let offset = *seen.entry(bytes).or_insert_with(|| {
            let o = data.len() as u64;
            data.extend_from_slice(bytes);
            o
        });
        if let Some(last) = entries.last_mut()
            && last.offset == offset
            && last.tile_id + last.run_length == id
        {
            last.run_length += 1;
            continue;
        }
        entries.push(Entry {
            tile_id: id,
            offset,
            length: bytes.len() as u64,
            run_length: 1,
        });
    }

    let mut dir = Vec::new();
    varint(&mut dir, entries.len() as u64);
    let mut last_id = 0;
    for e in &entries {
        varint(&mut dir, e.tile_id - last_id);
        last_id = e.tile_id;
    }
    for e in &entries {
        varint(&mut dir, e.run_length);
    }
    for e in &entries {
        varint(&mut dir, e.length);
    }
    for (i, e) in entries.iter().enumerate() {
        let contiguous = i > 0 && e.offset == entries[i - 1].offset + entries[i - 1].length;
        varint(&mut dir, if contiguous { 0 } else { e.offset + 1 });
    }
    if dir.len() > ROOT_LIMIT {
        return Err(IoError::Invalid(format!(
            "PMTiles: root directory of {} bytes exceeds {ROOT_LIMIT}; leaf directories are not implemented",
            dir.len()
        )));
    }

    let meta = serde_json::to_vec(&info.metadata).map_err(|e| IoError::Invalid(e.to_string()))?;
    let (zmin, zmax) = tiles
        .iter()
        .fold((u8::MAX, 0), |(lo, hi), t| (lo.min(t.0), hi.max(t.0)));
    let root_off = HEADER_LEN as u64;
    let meta_off = root_off + dir.len() as u64;
    let leaf_off = meta_off + meta.len() as u64;
    let data_off = leaf_off;

    let mut out = Vec::with_capacity(HEADER_LEN + dir.len() + meta.len() + data.len());
    out.extend_from_slice(b"PMTiles");
    out.push(3);
    for v in [
        root_off,
        dir.len() as u64,
        meta_off,
        meta.len() as u64,
        leaf_off,
        0,
        data_off,
        data.len() as u64,
        order.len() as u64,
        entries.len() as u64,
        seen.len() as u64,
    ] {
        out.extend_from_slice(&v.to_le_bytes());
    }
    // Clustered; internal compression none; tile compression none (the
    // image formats compress themselves); tile type; zoom range.
    out.extend_from_slice(&[1, 1, 1, info.tile_type as u8, zmin, zmax]);
    let [w, s, e, n] = info.bounds;
    for v in [w, s, e, n] {
        out.extend_from_slice(&e7(v));
    }
    out.push(info.center.2);
    out.extend_from_slice(&e7(info.center.0));
    out.extend_from_slice(&e7(info.center.1));
    debug_assert_eq!(out.len(), HEADER_LEN);
    out.extend_from_slice(&dir);
    out.extend_from_slice(&meta);
    out.extend_from_slice(&data);
    Ok(out)
}

/// Read one tile back (root directory only, no compression), for tests.
pub fn read_tile(file: &[u8], z: u8, x: u32, y: u32) -> Option<&[u8]> {
    if file.len() < HEADER_LEN || &file[..7] != b"PMTiles" || file[7] != 3 {
        return None;
    }
    let u64_at = |o: usize| u64::from_le_bytes(file[o..o + 8].try_into().unwrap());
    let (root_off, root_len, data_off) =
        (u64_at(8) as usize, u64_at(16) as usize, u64_at(56) as usize);
    let dir = file.get(root_off..root_off + root_len)?;
    let mut pos = 0;
    let mut next = || -> Option<u64> {
        let (mut v, mut shift) = (0_u64, 0);
        loop {
            let b = *dir.get(pos)?;
            pos += 1;
            v |= u64::from(b & 0x7f) << shift;
            if b < 0x80 {
                return Some(v);
            }
            shift += 7;
        }
    };
    let n = next()? as usize;
    let mut ids = Vec::with_capacity(n);
    let mut acc = 0;
    for _ in 0..n {
        acc += next()?;
        ids.push(acc);
    }
    let runs: Vec<u64> = (0..n).map(|_| next()).collect::<Option<_>>()?;
    let lens: Vec<u64> = (0..n).map(|_| next()).collect::<Option<_>>()?;
    let mut offs = Vec::with_capacity(n);
    for i in 0..n {
        let v = next()?;
        offs.push(if v == 0 && i > 0 {
            offs[i - 1] + lens[i - 1]
        } else {
            v - 1
        });
    }
    let want = tile_id(z, x, y);
    let i = (0..n).find(|&i| ids[i] <= want && want < ids[i] + runs[i])?;
    let start = data_off + offs[i] as usize;
    file.get(start..start + lens[i] as usize)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn info() -> PmTilesInfo {
        PmTilesInfo {
            tile_type: TileType::Webp,
            bounds: [-105.95, 40.45, -105.8, 40.58],
            center: (-105.875, 40.515, 12),
            metadata: serde_json::json!({"name": "test"}),
        }
    }

    #[test]
    fn round_trip_with_dedup_and_runs() {
        let a = vec![1_u8, 2, 3];
        let b = vec![9_u8; 5];
        // z1 ids: (0,0)=1, (0,1)=2, (1,1)=3, (1,0)=4.
        let tiles = vec![
            (1, 0, 0, a.clone()),
            (1, 0, 1, a.clone()),
            (1, 1, 1, b.clone()),
            (1, 1, 0, a.clone()),
            (0, 0, 0, b.clone()),
        ];
        let f = write_pmtiles(&tiles, &info()).unwrap();
        assert_eq!(&f[..8], b"PMTiles\x03");
        for (z, x, y, bytes) in &tiles {
            assert_eq!(
                read_tile(&f, *z, *x, *y),
                Some(bytes.as_slice()),
                "{z}/{x}/{y}"
            );
        }
        // Two distinct contents; ids 1-2 share a run.
        let u = |o: usize| u64::from_le_bytes(f[o..o + 8].try_into().unwrap());
        assert_eq!(
            (u(72), u(80), u(88)),
            (5, 4, 2),
            "addressed, entries, contents"
        );
        assert_eq!(u(64), 8, "tile data holds 3 + 5 bytes");
        assert_eq!((f[99], f[100], f[101]), (4, 0, 1), "webp, zoom 0-1");
        assert_eq!(read_tile(&f, 2, 0, 0), None);
    }

    #[test]
    fn rejects_bad_input() {
        assert!(write_pmtiles(&[], &info()).is_err());
        let dup = vec![(1, 0, 0, vec![1]), (1, 0, 0, vec![2])];
        assert!(write_pmtiles(&dup, &info()).is_err());
    }

    #[test]
    fn varints() {
        let mut v = Vec::new();
        varint(&mut v, 300);
        assert_eq!(v, [0xac, 0x02]);
    }
}
