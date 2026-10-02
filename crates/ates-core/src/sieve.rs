//! Removal of small raster polygons: a port of GDAL's `GDALSieveFilter`.
//!
//! AutoATES runs `gdal.SieveFilter` on its binary PRA raster. This module
//! reproduces that function so the core stays free of I/O. Sources: GDAL
//! v3.13.3 `alg/gdalsievefilter.cpp` and
//! `alg/gdalrasterpolygonenumerator.cpp` (MIT licence).
//!
//! The algorithm is ported step by step, including its order of operations,
//! because ties between equally large neighbours are broken by scan order:
//!
//! 1. Enumerate polygons (connected same-value regions) line by line, merging
//!    ids as regions join, and count their sizes.
//! 2. For every pair of touching polygons, record each one's largest
//!    neighbour (the first one found wins a tie).
//! 3. For each polygon smaller than the threshold, follow the chain of
//!    largest neighbours until one reaches the threshold. If none does, the
//!    polygon is left unchanged.
//! 4. Give every merged polygon its target's value.
//!
//! Two GDAL behaviours carry over. A pixel whose value equals
//! [`GP_NODATA_MARKER`] is treated as masked, like a masked pixel. Masked
//! pixels are never changed and never count as neighbours.

use ndarray::Array2;

/// GDAL's internal marker for masked pixels (`gdal_alg_priv.h`).
pub const GP_NODATA_MARKER: i64 = -51_502_112;

const MY_MAX_INT: i64 = i32::MAX as i64;

/// Which pixels count as touching.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Connectedness {
    /// Edge neighbours only.
    Four,
    /// Edge and diagonal neighbours.
    Eight,
}

/// `GDALRasterPolygonEnumeratorT<std::int64_t, IntEqualityTest>`.
struct Enumerator {
    eight: bool,
    id_map: Vec<i32>,
    value: Vec<i64>,
}

impl Enumerator {
    fn new_polygon(&mut self, v: i64) -> i32 {
        let id = i32::try_from(self.id_map.len()).expect("more than i32::MAX polygons");
        self.id_map.push(id);
        self.value.push(v);
        id
    }

    fn merge_polygon(&mut self, mut src: i32, dst_init: i32) {
        let map = &mut self.id_map;
        let at = |m: &[i32], i: i32| m[i as usize];
        let mut dst_final = dst_init;
        while at(map, dst_final) != dst_final {
            dst_final = at(map, dst_final);
        }
        let mut cur = dst_init;
        while at(map, cur) != cur {
            let next = at(map, cur);
            map[cur as usize] = dst_final;
            cur = next;
        }
        while at(map, src) != src {
            let next = at(map, src);
            map[src as usize] = dst_final;
            src = next;
        }
        map[src as usize] = dst_final;
    }

    fn complete_merges(&mut self) {
        let map = &mut self.id_map;
        for i in 0..map.len() {
            let mut id = map[i];
            while id != map[id as usize] {
                id = map[id as usize];
            }
            let mut cur = map[i];
            map[i] = id;
            while cur != map[cur as usize] {
                let next = map[cur as usize];
                map[cur as usize] = id;
                cur = next;
            }
        }
    }

    /// `ProcessLine`: assign polygon ids to `this`, given the previous line.
    fn process_line(&mut self, last: Option<(&[i64], &[i32])>, this: &[i64], this_id: &mut [i32]) {
        let n = this.len();
        let Some((last_val, last_id)) = last else {
            for i in 0..n {
                this_id[i] = if this[i] == GP_NODATA_MARKER {
                    -1
                } else if i == 0 || this[i] != this[i - 1] {
                    self.new_polygon(this[i])
                } else {
                    this_id[i - 1]
                };
            }
            return;
        };
        let eight = self.eight;
        for i in 0..n {
            let v = this[i];
            if v == GP_NODATA_MARKER {
                this_id[i] = -1;
            } else if i > 0 && v == this[i - 1] {
                this_id[i] = this_id[i - 1];
                let joins = |s: &Self, j: usize| {
                    last_val[j] == v
                        && s.id_map[last_id[j] as usize] != s.id_map[this_id[i] as usize]
                };
                if joins(self, i) {
                    self.merge_polygon(last_id[i], this_id[i]);
                }
                if eight && joins(self, i - 1) {
                    self.merge_polygon(last_id[i - 1], this_id[i]);
                }
                if eight && i + 1 < n && joins(self, i + 1) {
                    self.merge_polygon(last_id[i + 1], this_id[i]);
                }
            } else if last_val[i] == v {
                this_id[i] = last_id[i];
            } else if i > 0 && eight && last_val[i - 1] == v {
                this_id[i] = last_id[i - 1];
                if i + 1 < n
                    && last_val[i + 1] == v
                    && self.id_map[last_id[i + 1] as usize] != self.id_map[this_id[i] as usize]
                {
                    self.merge_polygon(last_id[i + 1], this_id[i]);
                }
            } else if i + 1 < n && eight && last_val[i + 1] == v {
                this_id[i] = last_id[i + 1];
            } else {
                this_id[i] = self.new_polygon(v);
            }
        }
    }
}

/// `CompareNeighbour`: update each polygon's largest neighbour.
fn compare_neighbour(id1: i32, id2: i32, map: &[i32], sizes: &[i32], big: &mut [i32]) {
    if id1 < 0 || id2 < 0 {
        return;
    }
    let (p1, p2) = (map[id1 as usize] as usize, map[id2 as usize] as usize);
    if p1 == p2 {
        return;
    }
    if big[p1] == -1 || sizes[big[p1] as usize] < sizes[p2] {
        big[p1] = p2 as i32;
    }
    if big[p2] == -1 || sizes[big[p2] as usize] < sizes[p1] {
        big[p2] = p1 as i32;
    }
}

/// Replace polygons smaller than `threshold` pixels with the value of their
/// largest neighbour, like `GDALSieveFilter(src, mask, dst, threshold,
/// connectedness)`. `mask` marks valid pixels (`true`); masked pixels keep
/// their value and are not part of any polygon.
///
/// # Panics
/// If `mask` has a different shape from `values`.
pub fn sieve_filter(
    values: &Array2<i16>,
    mask: Option<&Array2<bool>>,
    threshold: usize,
    connectedness: Connectedness,
) -> Array2<i16> {
    let (rows, cols) = values.dim();
    if let Some(m) = mask {
        assert_eq!(
            m.dim(),
            values.dim(),
            "sieve mask shape differs from values"
        );
    }
    // Per-line values as GDAL reads them: Int64, masked pixels marked.
    let line = |r: usize| -> Vec<i64> {
        (0..cols)
            .map(|c| match mask {
                Some(m) if !m[[r, c]] => GP_NODATA_MARKER,
                _ => i64::from(values[[r, c]]),
            })
            .collect()
    };

    // Pass 1: polygon ids per pixel (the second and third GDAL passes
    // recompute exactly these ids, so they are kept instead).
    let mut en = Enumerator {
        eight: connectedness == Connectedness::Eight,
        id_map: Vec::new(),
        value: Vec::new(),
    };
    let mut ids = Array2::<i32>::from_elem((rows, cols), -1);
    let mut sizes: Vec<i32> = Vec::new();
    let mut last: Option<(Vec<i64>, Vec<i32>)> = None;
    for r in 0..rows {
        let this = line(r);
        let mut this_id = vec![-1; cols];
        en.process_line(
            last.as_ref().map(|(v, i)| (v.as_slice(), i.as_slice())),
            &this,
            &mut this_id,
        );
        sizes.resize(en.id_map.len(), 0);
        for &id in &this_id {
            if id >= 0 && i64::from(sizes[id as usize]) < MY_MAX_INT {
                sizes[id as usize] += 1;
            }
        }
        for (c, &id) in this_id.iter().enumerate() {
            ids[[r, c]] = id;
        }
        last = Some((this, this_id));
    }
    en.complete_merges();
    if en.id_map.is_empty() {
        return values.clone();
    }

    // Fold fragment sizes into their final polygon.
    let map = &en.id_map;
    for i in 0..map.len() {
        let root = map[i] as usize;
        if root != i {
            let n = (i64::from(sizes[root]) + i64::from(sizes[i])).min(MY_MAX_INT);
            sizes[root] = n as i32;
            sizes[i] = 0;
        }
    }

    // Pass 2: largest neighbour of every polygon.
    let eight = connectedness == Connectedness::Eight;
    let mut big = vec![-1_i32; sizes.len()];
    for r in 0..rows {
        for c in 0..cols {
            let id = ids[[r, c]];
            if r > 0 {
                compare_neighbour(id, ids[[r - 1, c]], map, &sizes, &mut big);
                if c > 0 && eight {
                    compare_neighbour(id, ids[[r - 1, c - 1]], map, &sizes, &mut big);
                }
                if c + 1 < cols && eight {
                    compare_neighbour(id, ids[[r - 1, c + 1]], map, &sizes, &mut big);
                }
            }
            if c > 0 {
                compare_neighbour(id, ids[[r, c - 1]], map, &sizes, &mut big);
            }
        }
    }

    // Follow chains of small neighbours to a polygon at the threshold.
    let threshold = i32::try_from(threshold).unwrap_or(i32::MAX);
    for p in 0..sizes.len() {
        if map[p] as usize != p || en.value[p] == GP_NODATA_MARKER {
            continue;
        }
        if sizes[p] >= threshold {
            big[p] = -1;
            continue;
        }
        if big[p] == -1 {
            continue; // isolated small polygon: left unchanged
        }
        let mut visited = std::collections::BTreeSet::from([p as i32]);
        let mut fin = p as i32;
        let found = loop {
            fin = big[fin as usize];
            if fin < 0 {
                break false;
            }
            if sizes[fin as usize] >= threshold {
                break true;
            }
            if !visited.insert(fin) {
                break false;
            }
        };
        if !found {
            big[p] = -1;
            continue;
        }
        let mut cur = p;
        while big[cur] != fin {
            let next = big[cur] as usize;
            big[cur] = fin;
            cur = next;
        }
    }

    // Pass 3: write merged values; everything else keeps its input value.
    let mut out = values.clone();
    for ((r, c), &id) in ids.indexed_iter() {
        if id >= 0 {
            let p = map[id as usize] as usize;
            if big[p] != -1 {
                // Values come from i16 input, so they fit.
                out[[r, c]] = en.value[big[p] as usize] as i16;
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use ndarray::array;

    #[test]
    fn small_island_takes_surrounding_value() {
        let v = array![[0, 0, 0, 0], [0, 1, 1, 0], [0, 0, 0, 0], [0, 0, 0, 1]];
        let out = sieve_filter(&v, None, 3, Connectedness::Eight);
        assert_eq!(out, Array2::zeros((4, 4)));
        // Polygons at the threshold survive.
        let out = sieve_filter(&v, None, 2, Connectedness::Eight);
        assert_eq!(out[[1, 1]], 1);
        assert_eq!(out[[3, 3]], 0);
    }

    #[test]
    fn diagonal_pixels_join_only_with_eight_connectedness() {
        let v = array![[1, 0, 0], [0, 1, 0], [0, 0, 1]];
        let eight = sieve_filter(&v, None, 3, Connectedness::Eight);
        assert_eq!(eight, v, "one 3-pixel diagonal polygon is kept");
        let four = sieve_filter(&v, None, 3, Connectedness::Four);
        assert_eq!(four, Array2::zeros((3, 3)));
    }

    #[test]
    fn merges_into_largest_neighbour() {
        // The lone 9 touches a 2-region (6 px) and a 5-region (3 px).
        let v = array![[2, 2, 2, 5], [2, 9, 5, 5], [2, 2, 7, 7]];
        let out = sieve_filter(&v, None, 2, Connectedness::Four);
        assert_eq!(out[[1, 1]], 2);
        assert_eq!(out[[2, 2]], 7, "2-pixel polygon is at the threshold");
    }

    #[test]
    fn masked_pixels_are_kept_and_isolate() {
        let v = array![[0, 0, 0], [0, 1, 0], [0, 0, 0]];
        let mut mask = Array2::from_elem((3, 3), false);
        mask[[1, 1]] = true;
        // The 1 has no valid neighbours, so it stays; masked pixels stay too.
        assert_eq!(sieve_filter(&v, Some(&mask), 4, Connectedness::Eight), v);
    }

    #[test]
    fn chains_of_small_polygons_follow_to_a_large_one() {
        // The 1 (1 px) only touches the 2s (2 px), whose largest neighbour
        // is the 0s (3 px); with threshold 3 both end up as 0.
        let v = array![[1, 2, 2, 0, 0, 0]];
        let out = sieve_filter(&v, None, 3, Connectedness::Four);
        assert_eq!(out, Array2::zeros((1, 6)));
    }
}
