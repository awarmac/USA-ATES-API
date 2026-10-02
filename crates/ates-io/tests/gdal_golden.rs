//! Golden tests against GDAL: our Horn slope/aspect must match `gdaldem`,
//! and the reader/writer must round-trip georeferencing and provenance.
//!
//! Fixtures: `tests/fixtures/bow_summit` (see its README). Requires the GDAL
//! library (feature `gdal`).
#![cfg(feature = "gdal")]

use std::path::{Path, PathBuf};

use ates_core::terrain::{aspect_deg, slope_deg};
use ates_core::{BBox, Crs, Grid};
use ates_io::gdal_backend::{GdalDem, GeoTiffWriter, gdaldem_slope_aspect, read_grid};
use ates_io::{Band, DISCLAIMER, Provenance, RasterSink, RasterSource, Resampling, WindowRequest};
use gdal::{Dataset, Metadata};

const TOL_DEG: f64 = 1e-3;

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../tests/fixtures/bow_summit")
        .join(name)
}

/// Max abs difference over cells valid in both; panics on validity mismatch.
fn max_diff(ours: &Grid<f32>, reference: &Grid<f32>, circular: bool) -> (usize, f64) {
    assert_eq!(ours.data.dim(), reference.data.dim());
    let (mut n, mut max) = (0, 0.0_f64);
    for (i, (&a, &b)) in ours.data.iter().zip(reference.data.iter()).enumerate() {
        match (ours.is_nodata(a), reference.is_nodata(b)) {
            (true, true) => {}
            (false, false) => {
                let mut d = (f64::from(a) - f64::from(b)).abs();
                if circular {
                    d = d.min(360.0 - d);
                }
                n += 1;
                max = max.max(d);
            }
            (va, vb) => {
                panic!("validity differs at flat index {i}: ours nodata={va}, ref nodata={vb}")
            }
        }
    }
    (n, max)
}

#[test]
fn slope_and_aspect_match_committed_gdaldem_outputs() {
    let dem = read_grid(&fixture("dem.tif")).unwrap();
    assert_eq!(dem.crs, Crs::Epsg(32611));
    for (edges, sfx) in [(false, ""), (true, "_edges")] {
        let slope_ref = read_grid(&fixture(&format!("slope_gdaldem{sfx}.tif"))).unwrap();
        let aspect_ref = read_grid(&fixture(&format!("aspect_gdaldem{sfx}.tif"))).unwrap();
        let (n, d) = max_diff(&slope_deg(&dem, edges), &slope_ref, false);
        assert!(
            n > 25_000 && d <= TOL_DEG,
            "slope{sfx}: {n} cells, max diff {d}"
        );
        let (n, d) = max_diff(&aspect_deg(&dem, edges), &aspect_ref, true);
        assert!(
            n > 25_000 && d <= TOL_DEG,
            "aspect{sfx}: {n} cells, max diff {d}"
        );
    }
}

#[test]
fn warped_window_is_utm_and_matches_in_process_gdaldem() {
    // Centre of the fixture, ~51.70 N 116.50 W; 30 m cells, 1 km pad.
    let req = WindowRequest {
        bbox_wgs84: BBox::new(-116.52, 51.69, -116.48, 51.71),
        pad_m: 1000.0,
        dst_epsg: 32611,
        target_res_m: Some(30.0),
        resampling: Resampling::Bilinear,
    };
    let dem = GdalDem::new(fixture("dem.tif")).read_window(&req).unwrap();
    assert_eq!(dem.crs, Crs::Epsg(32611));
    assert_eq!(dem.transform.ew_res(), 30.0);
    assert_eq!(dem.transform.ns_res(), 30.0);
    // Snapped to the 30 m grid.
    assert_eq!(dem.transform.0[0] % 30.0, 0.0);
    assert_eq!(dem.transform.0[3] % 30.0, 0.0);
    // 2.78 km x 2.24 km request plus 1 km pad per side, snapped outward to
    // 30 m (checked with gdal.Warp on the same bounds).
    assert_eq!((dem.rows(), dem.cols()), (143, 161));
    let valid = dem.data.iter().filter(|&&v| !dem.is_nodata(v)).count();
    assert!(valid > dem.data.len() / 2, "only {valid} valid cells");

    let (slope_ref, aspect_ref) = gdaldem_slope_aspect(&dem, false).unwrap();
    let (_, d) = max_diff(&slope_deg(&dem, false), &slope_ref, false);
    assert!(d <= TOL_DEG, "slope max diff {d}");
    let (_, d) = max_diff(&aspect_deg(&dem, false), &aspect_ref, true);
    assert!(d <= TOL_DEG, "aspect max diff {d}");
}

#[test]
fn native_resolution_needs_projected_source() {
    let req = WindowRequest {
        bbox_wgs84: BBox::point(-116.5, 51.7),
        pad_m: 100.0,
        dst_epsg: 32611,
        target_res_m: None,
        resampling: Resampling::Bilinear,
    };
    // The fixture is projected (UTM 11N), so its own resolution is used.
    let dem = GdalDem::new(fixture("dem.tif")).read_window(&req).unwrap();
    assert!((dem.transform.ew_res() - 25.741_554).abs() < 1e-3);
}

#[test]
fn window_outside_dem_is_an_error() {
    let req = WindowRequest {
        bbox_wgs84: BBox::point(-110.0, 45.0),
        pad_m: 100.0,
        dst_epsg: 32612,
        target_res_m: Some(30.0),
        resampling: Resampling::Bilinear,
    };
    let err = GdalDem::new(fixture("dem.tif"))
        .read_window(&req)
        .unwrap_err();
    assert!(err.to_string().contains("no data"), "{err}");
}

#[test]
fn geotiff_round_trip_with_provenance() {
    let dem = read_grid(&fixture("dem.tif")).unwrap();
    let slope = slope_deg(&dem, false);
    let aspect = aspect_deg(&dem, false);
    let prov = Provenance {
        tool_version: "test".into(),
        config_source: "config/default.toml".into(),
        config_hash: "0123456789abcdef".into(),
        region: Some("bow_summit".into()),
        dem_source: "dem.tif".into(),
        product: "slope_deg and aspect_deg".into(),
    };
    let dir = std::env::temp_dir().join(format!("ates-io-test-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("terrain.tif");
    GeoTiffWriter
        .write(
            &[
                Band::f32("slope_deg", &slope),
                Band::f32("aspect_deg", &aspect),
            ],
            &path,
            &prov,
        )
        .unwrap();

    let ds = Dataset::open(&path).unwrap();
    assert_eq!(ds.raster_count(), 2);
    assert_eq!(
        ds.metadata_item("ATES_DISCLAIMER", "").as_deref(),
        Some(DISCLAIMER)
    );
    assert_eq!(
        ds.metadata_item("ATES_REGION", "").as_deref(),
        Some("bow_summit")
    );
    assert_eq!(
        ds.metadata_item("ATES_CONFIG_FNV1A64", "").as_deref(),
        Some("0123456789abcdef")
    );
    assert_eq!(
        ds.rasterband(2).unwrap().description().unwrap(),
        "aspect_deg"
    );

    let back = read_grid(&path).unwrap();
    assert_eq!(back.transform, slope.transform);
    assert_eq!(back.crs, Crs::Epsg(32611));
    assert_eq!(back.nodata, Some(-9999.0));
    assert_eq!(back.data, slope.data);
    std::fs::remove_dir_all(&dir).ok();
}
