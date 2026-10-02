//! Web Mercator map tiles: tile maths, PMTiles tile IDs, and the colours
//! used to draw ATES classes.
//!
//! Tiles follow the usual XYZ scheme: 256 x 256 pixels, y counted from the
//! north, zoom 0 = one tile for the world.

/// Pixels per tile side.
pub const TILE_SIZE: usize = 256;

/// Largest zoom whose tile IDs fit PMTiles' safe range.
pub const MAX_ZOOM: u8 = 26;

/// Earth radius used by Web Mercator (EPSG:3857), metres.
const R: f64 = 6_378_137.0;

/// Overlay colours (RGBA) for ATES classes 0-4. Classes 1-3 follow the
/// green / blue / black convention of ATES maps; class 4 is red; class 0 is
/// left transparent so the basemap shows. Alpha keeps the topo map readable.
pub const ATES_COLORS: [[u8; 4]; 5] = [
    [255, 255, 255, 0],
    [56, 168, 0, 150],
    [0, 92, 230, 150],
    [20, 20, 20, 165],
    [220, 30, 30, 170],
];

/// Fractional tile coordinates of a WGS 84 point at zoom `z`.
pub fn lonlat_to_tile(lon: f64, lat: f64, z: u8) -> (f64, f64) {
    let n = f64::from(1_u32 << z);
    let lat = lat.clamp(-85.051_128_78, 85.051_128_78).to_radians();
    let x = (lon + 180.0) / 360.0 * n;
    let y = (1.0 - (lat.tan() + 1.0 / lat.cos()).ln() / std::f64::consts::PI) / 2.0 * n;
    (x, y)
}

/// WGS 84 coordinates of fractional tile coordinates at zoom `z`.
pub fn tile_to_lonlat(x: f64, y: f64, z: u8) -> (f64, f64) {
    let n = f64::from(1_u32 << z);
    let lon = x / n * 360.0 - 180.0;
    let lat = (std::f64::consts::PI * (1.0 - 2.0 * y / n))
        .sinh()
        .atan()
        .to_degrees();
    (lon, lat)
}

/// Inclusive tile ranges `(x0, x1, y0, y1)` covering a WGS 84 bbox
/// `[west, south, east, north]` at zoom `z`.
pub fn tile_range(bbox: [f64; 4], z: u8) -> (u32, u32, u32, u32) {
    let max = (1_u32 << z) - 1;
    let clamp = |v: f64| (v.floor().max(0.0) as u32).min(max);
    let (x0, y0) = lonlat_to_tile(bbox[0], bbox[3], z);
    let (x1, y1) = lonlat_to_tile(bbox[2], bbox[1], z);
    (clamp(x0), clamp(x1), clamp(y0), clamp(y1))
}

/// Ground size of one tile pixel at latitude `lat` and zoom `z` (m).
pub fn pixel_size_m(lat: f64, z: u8) -> f64 {
    2.0 * std::f64::consts::PI * R * lat.to_radians().cos()
        / (TILE_SIZE as f64 * f64::from(1_u32 << z))
}

/// Smallest zoom whose pixels are no larger than `cell_m` at `lat`: the
/// zoom that shows the data at its own resolution.
pub fn native_zoom(cell_m: f64, lat: f64) -> u8 {
    (0..=MAX_ZOOM)
        .find(|&z| pixel_size_m(lat, z) <= cell_m)
        .unwrap_or(MAX_ZOOM)
}

/// PMTiles v3 tile ID: position on the Hilbert curve of zoom `z`, after
/// all tiles of lower zooms. Matches `zxyToTileId` in the PMTiles
/// reference implementation.
///
/// # Panics
/// If `z > 26` or `x`/`y` is outside the zoom level.
pub fn tile_id(z: u8, x: u32, y: u32) -> u64 {
    assert!(z <= MAX_ZOOM, "zoom {z} above {MAX_ZOOM}");
    let n = 1_u64 << z;
    assert!(
        u64::from(x) < n && u64::from(y) < n,
        "tile {x}/{y} outside zoom {z}"
    );
    let acc: u64 = (0..z).map(|a| 1_u64 << (2 * a)).sum();
    let (mut x, mut y) = (u64::from(x), u64::from(y));
    let mut d = 0;
    let mut s = n / 2;
    while s > 0 {
        let rx = u64::from(x & s > 0);
        let ry = u64::from(y & s > 0);
        d += s * s * ((3 * rx) ^ ry);
        if ry == 0 {
            if rx == 1 {
                x = n - 1 - x;
                y = n - 1 - y;
            }
            std::mem::swap(&mut x, &mut y);
        }
        s /= 2;
    }
    acc + d
}

/// RGBA pixels of a tile from the class of each pixel (row-major, `None`
/// for outside or nodata, drawn transparent).
pub fn render_classes(classes: &[Option<i16>]) -> Vec<u8> {
    let mut out = Vec::with_capacity(classes.len() * 4);
    for c in classes {
        let rgba = match c {
            Some(c @ 0..=4) => ATES_COLORS[*c as usize],
            _ => [0, 0, 0, 0],
        };
        out.extend_from_slice(&rgba);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tile_ids_match_the_pmtiles_spec() {
        // PMTiles v3 spec examples.
        assert_eq!(tile_id(0, 0, 0), 0);
        assert_eq!(tile_id(1, 0, 0), 1);
        assert_eq!(tile_id(1, 0, 1), 2);
        assert_eq!(tile_id(1, 1, 1), 3);
        assert_eq!(tile_id(1, 1, 0), 4);
        assert_eq!(tile_id(2, 0, 0), 5);
        // Every tile of a zoom gets a distinct id in its zoom's range.
        let mut ids: Vec<u64> = (0..8)
            .flat_map(|x| (0..8).map(move |y| tile_id(3, x, y)))
            .collect();
        ids.sort_unstable();
        assert_eq!(ids, (21..85).collect::<Vec<_>>());
    }

    #[test]
    fn mercator_round_trip() {
        let (x, y) = lonlat_to_tile(-105.8917, 40.5208, 14);
        // Cameron Pass is in tile 14/3372/6171 (checked with the standard
        // formula y = (1 - asinh(tan(lat)) / pi) / 2 * 2^z).
        assert_eq!((x.floor(), y.floor()), (3372.0, 6171.0));
        let (lon, lat) = tile_to_lonlat(x, y, 14);
        assert!((lon + 105.8917).abs() < 1e-9 && (lat - 40.5208).abs() < 1e-9);
        assert_eq!(tile_range([-180.0, -85.0, 180.0, 85.0], 1), (0, 1, 0, 1));
    }

    #[test]
    fn native_zoom_for_10m_at_cameron_pass() {
        // z14 pixels are about 7.3 m at 40.5 N; z13 about 14.5 m.
        assert_eq!(native_zoom(10.0, 40.5), 14);
        assert!(pixel_size_m(40.5, 14) < 10.0 && pixel_size_m(40.5, 13) > 10.0);
    }

    #[test]
    fn renders_palette_and_transparency() {
        let px = render_classes(&[Some(3), None, Some(-9999)]);
        assert_eq!(&px[..4], &ATES_COLORS[3]);
        assert_eq!(&px[4..], &[0; 8]);
    }
}
