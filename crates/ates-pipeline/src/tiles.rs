//! Web map tiles of a region's ATES classes.
//!
//! Each tile pixel takes the class of the grid cell under its centre
//! (nearest neighbour, exact for categorical data). Tile pixel centres are
//! projected into the grid's CRS with one PROJ call per tile, and tiles are
//! rendered in parallel. Fully transparent tiles are left out.

use std::ops::RangeInclusive;

use ates_core::tiles::{TILE_SIZE, native_zoom, render_classes, tile_range, tile_to_lonlat};
use ates_core::{Crs, Grid};
use ates_io::{IoError, Projector};
use rayon::prelude::*;

use crate::PipelineError;

/// One rendered tile: RGBA pixels, row-major.
#[derive(Debug, Clone)]
pub struct RenderedTile {
    pub z: u8,
    pub x: u32,
    pub y: u32,
    pub rgba: Vec<u8>,
}

/// WGS 84 bounds `[west, south, east, north]` of a grid's extent.
pub fn grid_bounds_wgs84(
    grid: &Grid<i16>,
    projector: &dyn Projector,
) -> Result<[f64; 4], PipelineError> {
    let epsg = epsg_of(grid)?;
    let g = &grid.transform;
    let (x0, y1) = (g.0[0], g.0[3]);
    let x1 = x0 + grid.cols() as f64 * g.ew_res();
    let y0 = y1 - grid.rows() as f64 * g.ns_res();
    // Corners and edge midpoints, so curved edges are covered.
    let (xm, ym) = ((x0 + x1) / 2.0, (y0 + y1) / 2.0);
    let pts = [
        (x0, y0),
        (x0, y1),
        (x1, y0),
        (x1, y1),
        (xm, y0),
        (xm, y1),
        (x0, ym),
        (x1, ym),
    ];
    let ll = projector.to_lonlat_many(&pts, epsg)?;
    let mut b = [f64::MAX, f64::MAX, f64::MIN, f64::MIN];
    for (lon, lat) in ll {
        b = [b[0].min(lon), b[1].min(lat), b[2].max(lon), b[3].max(lat)];
    }
    Ok(b)
}

fn epsg_of(grid: &Grid<i16>) -> Result<u32, PipelineError> {
    match grid.crs {
        Crs::Epsg(e) => Ok(e),
        _ => Err(PipelineError::Io(IoError::Invalid(
            "tiles need a grid with an EPSG CRS".into(),
        ))),
    }
}

/// Default zoom range: up to the zoom that shows the grid at its own
/// resolution, and four levels below it. Map clients overzoom beyond.
pub fn default_zooms(grid: &Grid<i16>, bounds: [f64; 4]) -> RangeInclusive<u8> {
    let lat = (bounds[1] + bounds[3]) / 2.0;
    let max = native_zoom(grid.transform.ew_res(), lat);
    max.saturating_sub(4)..=max
}

/// Render every non-empty tile of `classes` over `zooms`.
pub fn render_ates_tiles(
    classes: &Grid<i16>,
    bounds: [f64; 4],
    zooms: RangeInclusive<u8>,
    projector: &(dyn Projector + Sync),
) -> Result<Vec<RenderedTile>, PipelineError> {
    let epsg = epsg_of(classes)?;
    let mut keys = Vec::new();
    for z in zooms {
        let (x0, x1, y0, y1) = tile_range(bounds, z);
        for x in x0..=x1 {
            for y in y0..=y1 {
                keys.push((z, x, y));
            }
        }
    }
    let n = TILE_SIZE;
    let rendered: Result<Vec<Option<RenderedTile>>, IoError> = keys
        .par_iter()
        .map(|&(z, x, y)| {
            let mut ll = Vec::with_capacity(n * n);
            for py in 0..n {
                for px in 0..n {
                    let fx = f64::from(x) + (px as f64 + 0.5) / n as f64;
                    let fy = f64::from(y) + (py as f64 + 0.5) / n as f64;
                    ll.push(tile_to_lonlat(fx, fy, z));
                }
            }
            let xy = projector.lonlat_to_many(&ll, epsg)?;
            let cls: Vec<Option<i16>> = xy
                .iter()
                .map(|&(gx, gy)| classes.cell_at(gx, gy).map(|ix| classes.data[ix]))
                .collect();
            let rgba = render_classes(&cls);
            let visible = rgba.chunks_exact(4).any(|p| p[3] > 0);
            Ok(visible.then_some(RenderedTile { z, x, y, rgba }))
        })
        .collect();
    Ok(rendered?.into_iter().flatten().collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use ates_core::GeoTransform;
    use ndarray::Array2;

    /// Treats lon/lat as projected coordinates: a grid in degrees.
    struct Identity;

    impl Projector for Identity {
        fn lonlat_to(&self, lon: f64, lat: f64, _: u32) -> Result<(f64, f64), IoError> {
            Ok((lon, lat))
        }

        fn to_lonlat(&self, x: f64, y: f64, _: u32) -> Result<(f64, f64), IoError> {
            Ok((x, y))
        }
    }

    #[test]
    fn renders_only_tiles_over_the_grid() {
        // A 1 x 1 degree grid of class 3, from 0 to 1 E and 0 to 1 N.
        let g = Grid::new(
            Array2::from_elem((10, 10), 3_i16),
            GeoTransform::north_up(0.0, 1.0, 0.1, 0.1),
            Crs::Epsg(4326),
            Some(-9999.0),
        )
        .unwrap();
        let b = grid_bounds_wgs84(&g, &Identity).unwrap();
        assert_eq!(b, [0.0, 0.0, 1.0, 1.0]);
        let tiles = render_ates_tiles(&g, b, 2..=2, &Identity).unwrap();
        // At z2 the grid sits in one tile (x 2, y 1).
        assert_eq!(tiles.len(), 1);
        let t = &tiles[0];
        assert_eq!((t.z, t.x, t.y), (2, 2, 1));
        let opaque = t.rgba.chunks_exact(4).filter(|p| p[3] > 0).count();
        assert!(opaque > 0 && opaque < TILE_SIZE * TILE_SIZE);
        assert!(
            t.rgba
                .chunks_exact(4)
                .filter(|p| p[3] > 0)
                .all(|p| p == ates_core::tiles::ATES_COLORS[3])
        );
    }
}
