//! Region builds loaded into memory at startup.
//!
//! A region build is a directory written by `ates build-region`: an
//! `ates.tif` plus optional layers (`dem`, `forest`, `pra`,
//! `fp_travel_angle`, `overhead`) and a `manifest.toml`. Cameron Pass is
//! about 1.9 M cells, so a handful of layers fits comfortably in memory, and
//! requests never touch the disk.

use std::path::Path;

use ates_core::terrain::{aspect_deg, slope_deg};
use ates_core::{Crs, Grid};
use ates_io::gdal_backend::read_grid;
use ates_pipeline::route::RegionGrids;

/// One region build, ready to serve.
#[derive(Debug, Clone)]
pub struct Region {
    pub name: String,
    pub epsg: u32,
    /// The build's `manifest.toml` (empty if missing).
    pub manifest: toml::Table,
    pub grids: RegionGrids,
    pub forest: Option<Grid<f32>>,
    pub slope: Option<Grid<f32>>,
}

impl Region {
    /// Assemble a region from grids already in memory; slope and aspect
    /// (into `grids.aspect`) are derived from the DEM once, here.
    pub fn from_grids(
        name: impl Into<String>,
        manifest: toml::Table,
        mut grids: RegionGrids,
        forest: Option<Grid<f32>>,
    ) -> Result<Self, String> {
        let name = name.into();
        let Crs::Epsg(epsg) = grids.ates.crs else {
            return Err(format!("region `{name}`: ates.tif needs an EPSG CRS"));
        };
        let slope = grids.dem.as_ref().map(|d| slope_deg(d, false));
        if grids.aspect.is_none() {
            grids.aspect = grids.dem.as_ref().map(|d| aspect_deg(d, false));
        }
        Ok(Self {
            name,
            epsg,
            manifest,
            grids,
            forest,
            slope,
        })
    }

    /// `bbox_wgs84` from the manifest, if present.
    pub fn bbox_wgs84(&self) -> Option<[f64; 4]> {
        let v = self.manifest.get("bbox_wgs84")?.as_array()?;
        let f: Vec<f64> = v
            .iter()
            .filter_map(|x| x.as_float().or_else(|| x.as_integer().map(|i| i as f64)))
            .collect();
        f.try_into().ok()
    }

    /// The manifest's config fingerprint, for provenance.
    pub fn config_hash(&self) -> Option<String> {
        self.manifest
            .get("config_fnv1a64")
            .and_then(|v| v.as_str())
            .map(str::to_owned)
    }
}

fn to_i16(g: Grid<f32>) -> Grid<i16> {
    Grid {
        data: g.data.mapv(|v| if v.is_nan() { -9999 } else { v as i16 }),
        transform: g.transform,
        crs: g.crs,
        nodata: g.nodata,
    }
}

/// Load one region build directory.
pub fn load_region(dir: &Path) -> Result<Region, String> {
    let name = dir
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| format!("{}: not a region directory", dir.display()))?
        .to_owned();
    let layer = |file: &str| -> Result<Option<Grid<f32>>, String> {
        let p = dir.join(format!("{file}.tif"));
        if !p.exists() {
            return Ok(None);
        }
        read_grid(&p)
            .map(Some)
            .map_err(|e| format!("{}: {e}", p.display()))
    };
    let ates = layer("ates")?.ok_or_else(|| format!("{}: no ates.tif", dir.display()))?;
    let manifest = match std::fs::read_to_string(dir.join("manifest.toml")) {
        Ok(text) => text
            .parse::<toml::Table>()
            .map_err(|e| format!("{}/manifest.toml: {e}", dir.display()))?,
        Err(_) => toml::Table::new(),
    };
    let grids = RegionGrids {
        ates: to_i16(ates),
        dem: layer("dem")?,
        pra: layer("pra")?.map(to_i16),
        fp_travel_angle: layer("fp_travel_angle")?,
        overhead: layer("overhead")?.map(to_i16),
        aspect: None,
    };
    Region::from_grids(name, manifest, grids, layer("forest")?)
}

/// Load every region build under `data_dir` (each subdirectory with an
/// `ates.tif`), sorted by name.
pub fn load_regions(data_dir: &Path) -> Result<Vec<Region>, String> {
    let entries =
        std::fs::read_dir(data_dir).map_err(|e| format!("{}: {e}", data_dir.display()))?;
    let mut regions = Vec::new();
    for entry in entries {
        let path = entry.map_err(|e| e.to_string())?.path();
        if path.join("ates.tif").exists() {
            regions.push(load_region(&path)?);
        }
    }
    regions.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(regions)
}
