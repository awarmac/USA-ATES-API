//! `ates` command-line interface.
//!
//! - `point` / `area`: slope, aspect and slope-band proxy classes from a DEM
//!   (crude proxy, not ATES).
//! - `pra`: AutoATES v2.0 potential release areas from a DEM and an
//!   optional forest raster.
//! - `flowpy`: Flow-Py runout from release areas.
//! - `classify`: AutoATES v2.0 classification from PRA and Flow-Py rasters.
//! - `build-region`: the whole chain for a configured region, fetching its
//!   DEM and forest data, written as Cloud-Optimized GeoTIFFs.
//! - `sample`: read a raster (e.g. a region build) at one point.
//! - `route`: evaluate a GPX or GeoJSON route against a region build.
//! - `check-slope`: compare our slope/aspect with GDAL's gdaldem.

use std::error::Error;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use ates_core::autoates::OutputMode;
use ates_core::crs::utm_epsg_for;
use ates_core::terrain::{aspect_deg, slope_deg};
use ates_core::{BBox, Grid};
use ates_io::gdal_backend::{
    CogWriter, GdalDem, GdalFill, GdalProjector, GeoTiffWriter, ImageServerSource,
    gdaldem_slope_aspect, read_grid,
};
use ates_io::{Band, GridSource, Projector, RasterSink, RasterSource, WindowRequest};
use ates_pipeline::config::ForestTypeName;
use ates_pipeline::region::{INITIAL_PAD_M, RegionSources, build_region};
use ates_pipeline::route::{ATES_CLASS_NAMES, RegionGrids, evaluate_route, report_geojson};
use ates_pipeline::{
    AutoAtesInputs, Config, Params, compare, point, provenance, run_autoates, run_flowpy, run_pra,
    terrain,
};
use clap::{Args, Parser, Subcommand, ValueEnum};

const PROXY_PRODUCT: &str =
    "slope-band proxy classes, slope_deg, aspect_deg; NOT an ATES classification";
const PRA_PRODUCT: &str = "AutoATES v2.0 potential release areas (band 1 binary, band 2      likelihood 0-100); an input to ATES, NOT an ATES classification";
const FLOWPY_PRODUCT: &str = "Flow-Py runout with forest detrainment (AutoATES v2.0); an input      to ATES, NOT an ATES classification";
const REGION_PRODUCT: &str = "AutoATES v2.0 classification (ATES 0-4), region build: PRA, \
     Flow-Py, overhead and classifier computed by ates";
const AUTOATES_PRODUCT: &str =
    "AutoATES v2.0 classification (ATES 0-4) from supplied PRA and Flow-Py rasters";

#[derive(Parser)]
#[command(
    name = "ates",
    version,
    about = "Avalanche Terrain Exposure Scale estimator"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Print slope, aspect and the slope-band proxy class at one point.
    Point {
        #[command(flatten)]
        common: Common,
    },
    /// Write slope-band proxy classes, slope and aspect for an area to a
    /// 3-band GeoTIFF. The classes are a crude proxy, not ATES.
    Area {
        #[command(flatten)]
        common: Common,
        /// Output GeoTIFF path.
        #[arg(long)]
        out: PathBuf,
    },
    /// AutoATES v2.0 potential release areas from a DEM and an optional
    /// forest raster on the same grid.
    Pra(PraArgs),
    /// Flow-Py runout from release areas on the DEM grid. Writes one
    /// GeoTIFF per output into a directory, with upstream's file names.
    Flowpy(FlowpyArgs),
    /// AutoATES v2.0 classification from aligned rasters on the DEM grid:
    /// forest density, Flow-Py travel angle and cell counts, and PRA.
    Classify(ClassifyArgs),
    /// Build ATES for a configured region: fetch its DEM and forest data,
    /// run PRA, Flow-Py, overhead exposure and the classifier on one padded
    /// window, and write Cloud-Optimized GeoTIFFs plus a manifest.
    BuildRegion(BuildRegionArgs),
    /// Evaluate a route (GPX or GeoJSON) against a region build: length in
    /// each ATES class, and stretches with their terrain context.
    Route(RouteArgs),
    /// Print the value of every band of a raster at one point.
    Sample {
        /// Raster to read (any GDAL path).
        #[arg(long)]
        raster: PathBuf,
        /// Point as lon,lat in WGS 84 degrees.
        #[arg(long, value_parser = parse_center, allow_hyphen_values = true)]
        center: BBox,
    },
    /// Compare our slope/aspect with GDAL's gdaldem on the same DEM window.
    CheckSlope {
        #[command(flatten)]
        common: Common,
        /// Fail if any slope or aspect differs by more than this many degrees.
        #[arg(long, default_value_t = 1e-3)]
        tolerance_deg: f64,
    },
}

#[derive(Args)]
struct ClassifyArgs {
    /// DEM (projected, metres).
    #[arg(long)]
    dem: PathBuf,
    /// Forest density raster (units per --forest-type / region preset).
    #[arg(long)]
    forest: PathBuf,
    /// Flow-Py flow-path travel angle raster (AutoATES `FP_int16.tif`).
    #[arg(long)]
    flowpy_fp: PathBuf,
    /// Flow-Py cell-count raster (AutoATES uses its overhead/cell-count output).
    #[arg(long)]
    cell_count: PathBuf,
    /// Potential release areas (1 = release).
    #[arg(long)]
    pra: PathBuf,
    /// Output GeoTIFF (Int16, classes 0-4, nodata -9999).
    #[arg(long)]
    out: PathBuf,
    /// Forest type (overrides the region preset).
    #[arg(long, value_enum)]
    forest_type: Option<ForestTypeArg>,
    /// Reproduce AutoATES `ates_gen.tif` exactly (class 0 written as nodata).
    #[arg(long)]
    oracle_parity: bool,
    /// Also write every intermediate layer into this directory.
    #[arg(long)]
    intermediates: Option<PathBuf>,
    /// Region preset from the config.
    #[arg(long)]
    region: Option<String>,
    /// Config file.
    #[arg(long, default_value = "config/default.toml")]
    config: PathBuf,
}

#[derive(Args)]
struct PraArgs {
    /// DEM (projected, metres, square cells).
    #[arg(long)]
    dem: PathBuf,
    /// Forest density raster on the DEM grid. Without it, AutoATES's
    /// `no_forest` mode is used.
    #[arg(long)]
    forest: Option<PathBuf>,
    /// Forest type (overrides the region preset).
    #[arg(long, value_enum, requires = "forest")]
    forest_type: Option<ForestTypeArg>,
    /// Output GeoTIFF (Int16): band 1 binary PRA, band 2 likelihood 0-100.
    #[arg(long)]
    out: PathBuf,
    /// Also write the windshelter index and the unsieved binary PRA here.
    #[arg(long)]
    intermediates: Option<PathBuf>,
    /// Region preset from the config.
    #[arg(long)]
    region: Option<String>,
    /// Config file.
    #[arg(long, default_value = "config/default.toml")]
    config: PathBuf,
}

#[derive(Args)]
struct FlowpyArgs {
    /// DEM (projected, metres, square cells).
    #[arg(long)]
    dem: PathBuf,
    /// Release areas: cells > 0 start a path (band 1 of `ates pra` works).
    #[arg(long)]
    release: PathBuf,
    /// Flow-Py forest layer on the DEM grid, 0 (none) to 1 (dense). This is
    /// not the classifier's forest density raster. Omit for no forest.
    #[arg(long)]
    forest: Option<PathBuf>,
    /// Output directory.
    #[arg(long)]
    out_dir: PathBuf,
    /// Runout angle in degrees (overrides config `flowpy.alpha_deg`).
    #[arg(long)]
    alpha: Option<f64>,
    /// Holmgren exponent (overrides config `flowpy.exponent`).
    #[arg(long)]
    exponent: Option<i32>,
    /// Flux threshold (overrides config `flowpy.flux_threshold`).
    #[arg(long)]
    flux_threshold: Option<f64>,
    /// Maximum energy-line height in m (overrides config `flowpy.max_z_delta`).
    #[arg(long)]
    max_z: Option<f64>,
    /// Region preset from the config.
    #[arg(long)]
    region: Option<String>,
    /// Config file.
    #[arg(long, default_value = "config/default.toml")]
    config: PathBuf,
}

#[derive(Args)]
struct BuildRegionArgs {
    /// Region preset; its `site` table names the area and data sources.
    #[arg(long)]
    region: String,
    /// Output directory (default: data/regions/<region>).
    #[arg(long)]
    out_dir: Option<PathBuf>,
    /// Starting pad in metres (default: config `prep.pad_m`, else 1000).
    /// The build widens it until it exceeds the longest modelled runout.
    #[arg(long)]
    pad_m: Option<f64>,
    /// Override the region's bbox: west,south,east,north in WGS 84 degrees.
    #[arg(long, value_parser = parse_bbox, allow_hyphen_values = true)]
    bbox: Option<BBox>,
    /// Config file.
    #[arg(long, default_value = "config/default.toml")]
    config: PathBuf,
}

#[derive(Args)]
struct RouteArgs {
    /// Route file: .gpx, .geojson or .json (LineString / MultiLineString).
    #[arg(long)]
    file: PathBuf,
    /// Region whose build to use (reads data/regions/<region>).
    #[arg(long, required_unless_present = "region_dir")]
    region: Option<String>,
    /// Region build directory (overrides --region).
    #[arg(long)]
    region_dir: Option<PathBuf>,
    /// Write the full report as GeoJSON here.
    #[arg(long)]
    out: Option<PathBuf>,
}

#[derive(Clone, Copy, ValueEnum)]
enum ForestTypeArg {
    Bav,
    Pcc,
    Stems,
    Sen2ccc,
}

impl From<ForestTypeArg> for ForestTypeName {
    fn from(f: ForestTypeArg) -> Self {
        match f {
            ForestTypeArg::Bav => Self::Bav,
            ForestTypeArg::Pcc => Self::Pcc,
            ForestTypeArg::Stems => Self::Stems,
            ForestTypeArg::Sen2ccc => Self::Sen2ccc,
        }
    }
}

#[derive(Args)]
struct Common {
    /// DEM file (any GDAL-readable raster).
    #[arg(long)]
    dem: PathBuf,
    /// Area as west,south,east,north in WGS 84 degrees.
    #[arg(
        long,
        value_parser = parse_bbox,
        allow_hyphen_values = true,
        required_unless_present = "center",
        conflicts_with = "center"
    )]
    bbox: Option<BBox>,
    /// Point as lon,lat in WGS 84 degrees.
    #[arg(
        long,
        value_parser = parse_center,
        allow_hyphen_values = true,
        required_unless_present = "bbox"
    )]
    center: Option<BBox>,
    /// Padding in metres (overrides config `prep.pad_m`).
    #[arg(long)]
    pad_m: Option<f64>,
    /// Analysis cell size in metres (overrides config `dem.target_res_m`).
    #[arg(long)]
    res_m: Option<f64>,
    /// Region preset from the config.
    #[arg(long)]
    region: Option<String>,
    /// Config file.
    #[arg(long, default_value = "config/default.toml")]
    config: PathBuf,
}

impl Common {
    fn extent(&self) -> BBox {
        self.bbox
            .or(self.center)
            .expect("clap requires exactly one of --bbox / --center")
    }

    fn load(&self) -> Result<(Config, Params), Box<dyn Error>> {
        let config = Config::load(&self.config)?;
        let mut params = config.params(self.region.as_deref())?;
        if self.pad_m.is_some() {
            params.prep.pad_m = self.pad_m;
        }
        if self.res_m.is_some() {
            params.dem.target_res_m = self.res_m;
        }
        Ok((config, params))
    }
}

fn parse_floats<const N: usize>(s: &str) -> Result<[f64; N], String> {
    let v: Vec<f64> = s
        .split(',')
        .map(|p| p.trim().parse::<f64>().map_err(|e| format!("'{p}': {e}")))
        .collect::<Result<_, _>>()?;
    v.try_into()
        .map_err(|v: Vec<f64>| format!("expected {N} comma-separated numbers, got {}", v.len()))
}

fn parse_bbox(s: &str) -> Result<BBox, String> {
    let [w, s_, e, n] = parse_floats::<4>(s)?;
    if w >= e || s_ >= n {
        return Err("expected west < east and south < north".into());
    }
    Ok(BBox::new(w, s_, e, n))
}

fn parse_center(s: &str) -> Result<BBox, String> {
    let [lon, lat] = parse_floats::<2>(s)?;
    Ok(BBox::point(lon, lat))
}

fn run_point(common: &Common) -> Result<(), Box<dyn Error>> {
    let Some(center) = common.center else {
        return Err("`point` needs --center lon,lat".into());
    };
    let (_, params) = common.load()?;
    let (lon, lat) = center.center();
    let r = point(
        &GdalDem::new(&common.dem),
        &GdalProjector,
        lon,
        lat,
        &params,
    )?;
    let show = |v: Option<f32>| v.map_or("nodata".to_owned(), |v| format!("{v:.1}"));
    println!(
        "location:     {lon:.6}, {lat:.6} (EPSG:{} {:.1} E {:.1} N)",
        r.epsg, r.x, r.y
    );
    println!("elevation_m:  {}", show(r.elevation));
    println!("slope_deg:    {}", show(r.slope_deg));
    println!("aspect_deg:   {}", show(r.aspect_deg));
    println!(
        "slope_class:  {} (slope-band proxy; NOT an ATES rating)",
        r.slope_class.map_or("nodata".to_owned(), |c| c.to_string())
    );
    println!("note:         {}", ates_io::DISCLAIMER);
    Ok(())
}

fn run_area(common: &Common, out: &Path) -> Result<(), Box<dyn Error>> {
    let (config, params) = common.load()?;
    let source = GdalDem::new(&common.dem);
    let product = terrain(&source, common.extent(), &params)?;
    let prov = provenance(
        &config,
        common.region.as_deref(),
        &source.describe(),
        PROXY_PRODUCT,
    );
    GeoTiffWriter.write(
        &[
            Band::i16("slope_class_proxy", &product.slope_class),
            Band::f32("slope_deg", &product.slope_deg),
            Band::f32("aspect_deg", &product.aspect_deg),
        ],
        out,
        &prov,
    )?;
    eprintln!(
        "wrote {} ({}x{} cells, {} m) - {}",
        out.display(),
        product.slope_deg.rows(),
        product.slope_deg.cols(),
        product.slope_deg.transform.ew_res(),
        ates_io::DISCLAIMER
    );
    Ok(())
}

fn run_classify(a: &ClassifyArgs) -> Result<(), Box<dyn Error>> {
    let config = Config::load(&a.config)?;
    let mut params = config.params(a.region.as_deref())?;
    if let Some(f) = a.forest_type {
        params.classify.forest_type = Some(f.into());
    }
    let read = |p: &PathBuf| read_grid(p).map_err(|e| format!("{}: {e}", p.display()));
    let (dem, forest, fp, cc, pra) = (
        read(&a.dem)?,
        read(&a.forest)?,
        read(&a.flowpy_fp)?,
        read(&a.cell_count)?,
        read(&a.pra)?,
    );
    let mode = if a.oracle_parity {
        OutputMode::OracleParity
    } else {
        OutputMode::Product
    };
    let inputs = AutoAtesInputs {
        dem: &dem,
        forest: &forest,
        flowpy_fp: &fp,
        cell_count: &cc,
        pra: &pra,
    };
    let layers = run_autoates(inputs, &params, &GdalFill, mode)?;
    let prov = provenance(
        &config,
        a.region.as_deref(),
        &a.dem.display().to_string(),
        AUTOATES_PRODUCT,
    );
    GeoTiffWriter.write(&[Band::i16("ates_class", &layers.ates)], &a.out, &prov)?;

    if let Some(dir) = &a.intermediates {
        std::fs::create_dir_all(dir)?;
        let on_grid = |data: ndarray::Array2<i16>| Grid {
            data,
            ..layers.ates.clone()
        };
        let named = [
            ("slope", layers.slope_class.clone()),
            ("slope_smooth", layers.slope_smooth.clone()),
            ("flowpy", on_grid(layers.flowpy.clone())),
            ("cellcount_reclass", on_grid(layers.cellcount.clone())),
            ("forest_reclass", on_grid(layers.forest.clone())),
            ("SZ_reclass", on_grid(layers.pra.clone())),
            ("merge_new", on_grid(layers.merge_new.clone())),
            ("merge_all", on_grid(layers.merge_all.clone())),
            ("cleanup_mask", on_grid(layers.cleanup_mask.mapv(i16::from))),
            ("ates", layers.ates.clone()),
        ];
        for (name, grid) in &named {
            let path = dir.join(format!("{name}.tif"));
            GeoTiffWriter.write(&[Band::i16(name, grid)], &path, &prov)?;
        }
    }
    let mut counts = [0_usize; 6];
    for &v in &layers.ates.data {
        counts[if (0..=4).contains(&v) { v as usize } else { 5 }] += 1;
    }
    eprintln!(
        "wrote {}: cells per class 0-4 {:?}, nodata {} - {}",
        a.out.display(),
        &counts[..5],
        counts[5],
        ates_io::DISCLAIMER
    );
    Ok(())
}

fn run_pra_cmd(a: &PraArgs) -> Result<(), Box<dyn Error>> {
    let config = Config::load(&a.config)?;
    let mut params = config.params(a.region.as_deref())?;
    if let Some(f) = a.forest_type {
        params.classify.forest_type = Some(f.into());
    }
    let read = |p: &PathBuf| read_grid(p).map_err(|e| format!("{}: {e}", p.display()));
    let dem = read(&a.dem)?;
    let forest = a.forest.as_ref().map(read).transpose()?;
    let layers = run_pra(&dem, forest.as_ref(), &params)?;
    let prov = provenance(
        &config,
        a.region.as_deref(),
        &a.dem.display().to_string(),
        PRA_PRODUCT,
    );
    GeoTiffWriter.write(
        &[
            Band::i16("pra_binary", &layers.binary),
            Band::i16("pra_continuous", &layers.continuous),
        ],
        &a.out,
        &prov,
    )?;
    if let Some(dir) = &a.intermediates {
        std::fs::create_dir_all(dir)?;
        let ws = dir.join("windshelter.tif");
        GeoTiffWriter.write(&[Band::f32("windshelter", &layers.windshelter)], &ws, &prov)?;
        let unsieved = dir.join("pra_binary_unsieved.tif");
        GeoTiffWriter.write(
            &[Band::i16("pra_binary_unsieved", &layers.binary_unsieved)],
            &unsieved,
            &prov,
        )?;
    }
    let release = layers.binary.data.iter().filter(|&&v| v == 1).count();
    eprintln!(
        "wrote {}: {release} of {} cells are potential release areas - {}",
        a.out.display(),
        layers.binary.data.len(),
        ates_io::DISCLAIMER
    );
    Ok(())
}

fn run_flowpy_cmd(a: &FlowpyArgs) -> Result<(), Box<dyn Error>> {
    let config = Config::load(&a.config)?;
    let mut params = config.params(a.region.as_deref())?;
    let f = &mut params.flowpy;
    f.alpha_deg = a.alpha.or(f.alpha_deg);
    f.exponent = a.exponent.or(f.exponent);
    f.flux_threshold = a.flux_threshold.or(f.flux_threshold);
    f.max_z_delta = a.max_z.or(f.max_z_delta);
    let read = |p: &PathBuf| read_grid(p).map_err(|e| format!("{}: {e}", p.display()));
    let dem = read(&a.dem)?;
    let release = read(&a.release)?;
    let forest = a.forest.as_ref().map(read).transpose()?;
    let started = std::time::Instant::now();
    let out = run_flowpy(&dem, &release, forest.as_ref(), &params)?;
    let elapsed = started.elapsed().as_secs_f64();
    let prov = provenance(
        &config,
        a.region.as_deref(),
        &a.dem.display().to_string(),
        FLOWPY_PRODUCT,
    );
    std::fs::create_dir_all(&a.out_dir)?;
    for (name, grid) in [
        ("FP_travel_angle", &out.fp_travel_angle),
        ("cell_counts", &out.cell_counts),
        ("z_delta", &out.z_delta),
        ("flux", &out.flux),
        ("z_delta_sum", &out.z_delta_sum),
        // Upstream's name for the minimum flow-path distance.
        ("SL_travel_angle", &out.fp_distance),
    ] {
        let path = a.out_dir.join(format!("{name}.tif"));
        GeoTiffWriter.write(&[Band::f32(name, grid)], &path, &prov)?;
    }
    let reached = out.cell_counts.data.iter().filter(|&&c| c > 0.0).count();
    eprintln!(
        "wrote {} in {elapsed:.1} s: {reached} cells reached by runout - {}",
        a.out_dir.display(),
        ates_io::DISCLAIMER
    );
    Ok(())
}

fn run_build_region(a: &BuildRegionArgs) -> Result<(), Box<dyn Error>> {
    let config = Config::load(&a.config)?;
    let params = config.params(Some(&a.region))?;
    let site = &params.site;
    let need = |v: Option<&String>, key: &str| {
        v.cloned()
            .ok_or_else(|| format!("region `{}` has no `site.{key}`", a.region))
    };
    let bbox = match (a.bbox, site.bbox) {
        (Some(b), _) => b,
        (None, Some([w, s, e, n])) => BBox::new(w, s, e, n),
        (None, None) => return Err(format!("region `{}` has no `site.bbox`", a.region).into()),
    };
    let dem = GdalDem::new(need(site.dem_path.as_ref(), "dem_path")?);
    let forest = ImageServerSource {
        url: need(site.forest_service.as_ref(), "forest_service")?,
        where_clause: site.forest_where.clone(),
        invalid: site.forest_invalid.clone(),
    };
    let fp = params.flowpy_params()?;
    let pra = params.pra_params(params.dem.target_res_m.unwrap_or(f64::NAN), true)?;
    let autoates = params.autoates()?;
    eprintln!(
        "building `{}` {:?}\n  Flow-Py: alpha {} deg, exponent {}, flux threshold {}, max z_delta {} m, \
         forest {:?}\n  classifier: forest type {:?}, TREE {}/{}/{}",
        a.region,
        [bbox.min_x, bbox.min_y, bbox.max_x, bbox.max_y],
        fp.alpha_deg,
        fp.exponent,
        fp.flux_threshold,
        fp.max_z_delta,
        params.flowpy.forest,
        params.forest_type()?,
        autoates.forest.tree1,
        autoates.forest.tree2,
        autoates.forest.tree3,
    );
    let pad = a.pad_m.or(params.prep.pad_m).unwrap_or(INITIAL_PAD_M);
    let started = std::time::Instant::now();
    let b = build_region(
        RegionSources {
            dem: &dem,
            forest: &forest,
        },
        bbox,
        &params,
        &GdalFill,
        pad,
    )?;
    for t in &b.attempts {
        eprintln!(
            "  pad {:.0} m: longest modelled runout {:.0} m",
            t.pad_m, t.max_runout_m
        );
    }
    if !b.pad_sufficient {
        eprintln!("  WARNING: the pad did not exceed the longest runout; edges may miss runout");
    }

    let out_dir = a
        .out_dir
        .clone()
        .unwrap_or_else(|| Path::new("data/regions").join(&a.region));
    std::fs::create_dir_all(&out_dir)?;
    let sources = format!("DEM: {}; forest: {}", dem.describe(), forest.describe());
    let prov = provenance(&config, Some(&a.region), &sources, REGION_PRODUCT);
    let l = &b.layers;
    let path = |file: &str| out_dir.join(format!("{file}.tif"));
    CogWriter.write(&[Band::i16("ates_class", &l.ates)], &path("ates"), &prov)?;
    CogWriter.write(
        &[
            Band::i16("pra_binary", &l.pra_binary),
            Band::i16("pra_continuous", &l.pra_continuous),
        ],
        &path("pra"),
        &prov,
    )?;
    CogWriter.write(
        &[Band::i16("overhead", &l.overhead)],
        &path("overhead"),
        &prov,
    )?;
    for (file, g) in [
        ("dem", &l.dem),
        ("forest", &l.forest),
        ("fp_travel_angle", &l.fp_travel_angle),
        ("cell_counts", &l.cell_counts),
        ("z_delta", &l.z_delta),
    ] {
        CogWriter.write(
            &[Band::f32(file, g)],
            &out_dir.join(format!("{file}.tif")),
            &prov,
        )?;
    }

    let mut counts = [0_usize; 6];
    for &v in &l.ates.data {
        counts[if (0..=4).contains(&v) { v as usize } else { 5 }] += 1;
    }
    let attempts: Vec<String> = b
        .attempts
        .iter()
        .map(|t| {
            format!(
                "{{ pad_m = {}, max_runout_m = {:.1} }}",
                t.pad_m, t.max_runout_m
            )
        })
        .collect();
    let timings: Vec<String> = b
        .timings
        .iter()
        .map(|(n, d)| format!("\"{n}\" = {:.1}", d.as_secs_f64()))
        .collect();
    let cauchy = |c: ates_core::pra::Cauchy| format!("[{}, {}, {}]", c.a, c.b, c.c);
    let pra_keys = [
        format!("windshelter_radius_cells = {}", pra.radius_cells),
        format!("windshelter_prob = {}", pra.prob),
        format!("wind_dir_deg = {}", pra.wind_dir_deg),
        format!("wind_tol_deg = {}", pra.wind_tol_deg),
        format!("threshold = {}", pra.threshold),
        format!("sieve_cells = {}", pra.sieve_cells),
        format!("slope_cauchy = {}", cauchy(pra.slope)),
        format!("windshelter_cauchy = {}", cauchy(pra.windshelter)),
        format!("forest_cauchy = {}", cauchy(pra.forest)),
    ]
    .join("\n");
    let s = &autoates.slope;
    let f = &autoates.forest;
    let classify_keys = [
        format!("forest_type = \"{:?}\"", params.forest_type()?),
        format!("sat = [{}, {}, {}, {}]", s.sat01, s.sat12, s.sat23, s.sat34),
        format!("win_size = {}", s.win_size),
        format!(
            "aat = [{}, {}, {}]",
            autoates.aat1, autoates.aat2, autoates.aat3
        ),
        format!("tree = [{}, {}, {}]", f.tree1, f.tree2, f.tree3),
        format!("overhead_thresholds = [{}, {}]", autoates.cc1, autoates.cc2),
        format!("isl_size_m2 = {}", autoates.isl_size_m2),
    ]
    .join("\n");
    let manifest = format!(
        "# Region build manifest. {disclaimer}\n\
         region = \"{region}\"\n\
         tool_version = \"{version}\"\n\
         config = \"{cfg}\"\n\
         config_fnv1a64 = \"{hash}\"\n\
         bbox_wgs84 = [{w}, {s}, {e}, {n}]\n\
         crs = \"{crs:?}\"\n\
         cell_size_m = {res}\n\
         rows = {rows}\n\
         cols = {cols}\n\
         dem_source = \"{dem_src}\"\n\
         forest_source = \"{forest_src}\"\n\
         release_cells_in_window = {release}\n\
         pad_sufficient = {pad_ok}\n\
         pad_attempts = [{attempts}]\n\
         cells_per_class_0_to_4 = {classes:?}\n\
         nodata_cells = {nodata}\n\n\
         [flowpy]\nalpha_deg = {alpha}\nexponent = {exp}\nflux_threshold = {flux}\nmax_z_delta = {maxz}\n\
         forest = \"{fpforest:?}\"\n\n\
         [pra]\n{pra_keys}\n\n\
         [classify]\n{classify_keys}\n\n\
         # Seconds per step, for the final pad attempt.\n\
         [timings_s]\n{timings}\n",
        disclaimer = ates_io::DISCLAIMER,
        region = a.region,
        version = ates_pipeline::TOOL_VERSION,
        cfg = config.source.replace('\\', "/"),
        hash = config.hash,
        w = bbox.min_x,
        s = bbox.min_y,
        e = bbox.max_x,
        n = bbox.max_y,
        crs = l.ates.crs,
        res = l.ates.transform.ew_res(),
        rows = l.ates.rows(),
        cols = l.ates.cols(),
        dem_src = dem.describe(),
        forest_src = forest.describe(),
        release = b.release_cells,
        pad_ok = b.pad_sufficient,
        attempts = attempts.join(", "),
        classes = &counts[..5],
        nodata = counts[5],
        alpha = fp.alpha_deg,
        exp = fp.exponent,
        flux = fp.flux_threshold,
        maxz = fp.max_z_delta,
        fpforest = params.flowpy.forest,
        pra_keys = pra_keys,
        classify_keys = classify_keys,
        timings = timings.join("\n"),
    );
    std::fs::write(out_dir.join("manifest.toml"), manifest)?;
    eprintln!(
        "wrote {} in {:.0} s: {}x{} cells, cells per class 0-4 {:?}, nodata {} - {}",
        out_dir.display(),
        started.elapsed().as_secs_f64(),
        l.ates.rows(),
        l.ates.cols(),
        &counts[..5],
        counts[5],
        ates_io::DISCLAIMER
    );
    Ok(())
}

fn run_route(a: &RouteArgs) -> Result<(), Box<dyn Error>> {
    let dir = match (&a.region_dir, &a.region) {
        (Some(d), _) => d.clone(),
        (None, Some(r)) => Path::new("data/regions").join(r),
        (None, None) => unreachable!("clap requires --region or --region-dir"),
    };
    let i16_grid = |g: Grid<f32>| Grid {
        data: g.data.mapv(|v| if v.is_nan() { -9999 } else { v as i16 }),
        transform: g.transform,
        crs: g.crs,
        nodata: g.nodata,
    };
    let layer = |name: &str| -> Result<Option<Grid<f32>>, Box<dyn Error>> {
        let p = dir.join(format!("{name}.tif"));
        if p.exists() {
            Ok(Some(
                read_grid(&p).map_err(|e| format!("{}: {e}", p.display()))?,
            ))
        } else {
            Ok(None)
        }
    };
    let ates = layer("ates")?.ok_or_else(|| {
        format!(
            "no ates.tif in {}; run `ates build-region` first",
            dir.display()
        )
    })?;
    let grids = RegionGrids {
        ates: i16_grid(ates),
        dem: layer("dem")?,
        pra: layer("pra")?.map(i16_grid),
        fp_travel_angle: layer("fp_travel_angle")?,
        overhead: layer("overhead")?.map(i16_grid),
        aspect: None,
    };
    let parts = ates_io::route_file::read_route(&a.file)?;
    let result = evaluate_route(&parts, &grids, &GdalProjector)?;
    let rep = &result.report;

    let km = |m: f64| m / 1000.0;
    let pct = |m: f64| 100.0 * m / rep.total_m;
    println!(
        "route: {:.2} km in {} part(s), region build {}",
        km(rep.total_m),
        parts.len(),
        dir.display()
    );
    for (c, &m) in rep.class_m.iter().enumerate() {
        if m > 0.0 {
            println!(
                "  class {c} {:<22} {:>7.2} km  {:>5.1} %",
                ATES_CLASS_NAMES[c],
                km(m),
                pct(m)
            );
        }
    }
    if rep.nodata_m > 0.0 {
        println!(
            "  no class (nodata)              {:>7.2} km",
            km(rep.nodata_m)
        );
    }
    if rep.outside_m > 0.0 {
        println!(
            "  outside the region             {:>7.2} km",
            km(rep.outside_m)
        );
    }
    println!(
        "  in modelled release areas {:.0} m; on modelled avalanche paths {:.0} m",
        rep.release_area_m, rep.avalanche_path_m
    );
    let exposed: Vec<_> = rep
        .stretches
        .iter()
        .filter(|s| s.class.is_some_and(|c| c >= 3))
        .collect();
    if !exposed.is_empty() {
        println!("  class 3-4 stretches (distance along route):");
        for s in exposed {
            let elev = match (s.elevation_min_m, s.elevation_max_m) {
                (Some(lo), Some(hi)) => format!("{lo:.0}-{hi:.0} m"),
                _ => "-".into(),
            };
            println!(
                "    {:>6.0}-{:<6.0} m  class {}  {:>5.0} m long  aspect {:<2}  elevation {elev}",
                s.start_m,
                s.end_m,
                s.class.unwrap_or(-1),
                s.length_m(),
                s.dominant_aspect().map_or("-", |a| a.as_str()),
            );
        }
    }
    println!("note: {}", ates_io::DISCLAIMER);

    if let Some(out) = &a.out {
        let mut meta = serde_json::Map::new();
        meta.insert(
            "route_file".into(),
            a.file.display().to_string().replace('\\', "/").into(),
        );
        meta.insert(
            "tool_version".into(),
            ates_pipeline::TOOL_VERSION.to_owned().into(),
        );
        if let Ok(text) = std::fs::read_to_string(dir.join("manifest.toml"))
            && let Ok(manifest) = text.parse::<toml::Table>()
        {
            meta.insert("region_manifest".into(), serde_json::to_value(manifest)?);
        }
        let gj = report_geojson(&result, &GdalProjector, meta)?;
        std::fs::write(out, serde_json::to_string_pretty(&gj)?)?;
        eprintln!("wrote {}", out.display());
    }
    Ok(())
}

fn run_sample(raster: &Path, center: BBox) -> Result<(), Box<dyn Error>> {
    let (lon, lat) = center.center();
    let first = read_grid(raster)?;
    let ates_core::Crs::Epsg(epsg) = first.crs else {
        return Err("sample needs a raster with an EPSG CRS".into());
    };
    let (x, y) = GdalProjector.lonlat_to(lon, lat, epsg)?;
    let (r, c) = first
        .cell_at(x, y)
        .ok_or_else(|| format!("({lon}, {lat}) is outside {}", raster.display()))?;
    println!("location: {lon:.6}, {lat:.6} (EPSG:{epsg} {x:.1} E {y:.1} N, row {r} col {c})");
    for (i, (name, v)) in ates_io::gdal_backend::read_bands_at(raster, r, c)?
        .into_iter()
        .enumerate()
    {
        println!("band {}: {name} = {v}", i + 1);
    }
    println!("note: {}", ates_io::DISCLAIMER);
    Ok(())
}

fn run_check(common: &Common, tolerance_deg: f64) -> Result<bool, Box<dyn Error>> {
    let (_, params) = common.load()?;
    let bbox = common.extent();
    let (lon, lat) = bbox.center();
    let req = WindowRequest {
        bbox_wgs84: bbox,
        pad_m: params.pad_m()?,
        dst_epsg: utm_epsg_for(lon, lat)?,
        target_res_m: params.dem.target_res_m,
        resampling: params.dem.resampling,
    };
    let dem = GdalDem::new(&common.dem).read_window(&req)?;
    let edges = params.terrain.compute_edges;
    let (ref_slope, ref_aspect) = gdaldem_slope_aspect(&dem, edges)?;
    let slope = compare(&slope_deg(&dem, edges), &ref_slope, false);
    let aspect = compare(&aspect_deg(&dem, edges), &ref_aspect, true);
    println!(
        "window: {}x{} cells at {} m",
        dem.rows(),
        dem.cols(),
        dem.transform.ew_res()
    );
    println!("slope:  {slope:?}");
    println!("aspect: {aspect:?}");
    let ok = [slope, aspect]
        .iter()
        .all(|a| a.validity_mismatches == 0 && a.max_abs_diff <= tolerance_deg);
    println!("{}", if ok { "PASS" } else { "FAIL" });
    Ok(ok)
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let result = match &cli.command {
        Command::Point { common } => run_point(common).map(|()| true),
        Command::Area { common, out } => run_area(common, out).map(|()| true),
        Command::Pra(a) => run_pra_cmd(a).map(|()| true),
        Command::Flowpy(a) => run_flowpy_cmd(a).map(|()| true),
        Command::Classify(a) => run_classify(a).map(|()| true),
        Command::BuildRegion(a) => run_build_region(a).map(|()| true),
        Command::Sample { raster, center } => run_sample(raster, *center).map(|()| true),
        Command::Route(a) => run_route(a).map(|()| true),
        Command::CheckSlope {
            common,
            tolerance_deg,
        } => run_check(common, *tolerance_deg),
    };
    match result {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => ExitCode::FAILURE,
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn cli_definition_is_valid() {
        Cli::command().debug_assert();
    }

    #[test]
    fn parses_extents() {
        assert_eq!(
            parse_bbox("-116.6,51.6,-116.4,51.8").unwrap(),
            BBox::new(-116.6, 51.6, -116.4, 51.8)
        );
        assert!(parse_bbox("-116.4,51.6,-116.6,51.8").is_err());
        assert!(parse_bbox("1,2,3").is_err());
        assert_eq!(
            parse_center("-116.5, 51.7").unwrap(),
            BBox::point(-116.5, 51.7)
        );
        assert!(parse_center("x,1").is_err());
    }

    #[test]
    fn classify_args_parse() {
        let args = [
            "ates",
            "classify",
            "--dem",
            "d.tif",
            "--forest",
            "f.tif",
            "--flowpy-fp",
            "fp.tif",
            "--cell-count",
            "cc.tif",
            "--pra",
            "p.tif",
            "--out",
            "o.tif",
            "--forest-type",
            "pcc",
            "--oracle-parity",
        ];
        let Command::Classify(a) = Cli::try_parse_from(args).unwrap().command else {
            panic!("expected classify");
        };
        assert!(a.oracle_parity);
        assert!(matches!(a.forest_type, Some(ForestTypeArg::Pcc)));
        assert!(
            Cli::try_parse_from(&args[..12]).is_err(),
            "--out is required"
        );
    }

    #[test]
    fn pra_args_parse() {
        let base = ["ates", "pra", "--dem", "d.tif", "--out", "o.tif"];
        let Command::Pra(a) = Cli::try_parse_from(base).unwrap().command else {
            panic!("expected pra");
        };
        assert!(a.forest.is_none());
        let typed = [&base[..], &["--forest-type", "stems"]].concat();
        assert!(
            Cli::try_parse_from(typed).is_err(),
            "--forest-type needs --forest"
        );
        let full = [&base[..], &["--forest", "f.tif", "--forest-type", "stems"]].concat();
        assert!(Cli::try_parse_from(full).is_ok());
    }

    #[test]
    fn flowpy_args_parse() {
        let args = [
            "ates",
            "flowpy",
            "--dem",
            "d.tif",
            "--release",
            "r.tif",
            "--out-dir",
            "o",
            "--alpha",
            "25",
        ];
        let Command::Flowpy(a) = Cli::try_parse_from(args).unwrap().command else {
            panic!("expected flowpy");
        };
        assert_eq!(a.alpha, Some(25.0));
        assert!(a.forest.is_none() && a.exponent.is_none());
        assert!(
            Cli::try_parse_from(&args[..6]).is_err(),
            "--out-dir is required"
        );
    }

    #[test]
    fn build_region_args_parse() {
        let args = ["ates", "build-region", "--region", "cameron_pass"];
        let Command::BuildRegion(a) = Cli::try_parse_from(args).unwrap().command else {
            panic!("expected build-region");
        };
        assert_eq!(a.region, "cameron_pass");
        assert!(a.out_dir.is_none() && a.pad_m.is_none() && a.bbox.is_none());
        assert!(
            Cli::try_parse_from(&args[..2]).is_err(),
            "--region is required"
        );
    }

    #[test]
    fn route_args_parse() {
        let ok = [
            "ates",
            "route",
            "--file",
            "r.gpx",
            "--region",
            "cameron_pass",
        ];
        assert!(Cli::try_parse_from(ok).is_ok());
        let dir = ["ates", "route", "--file", "r.gpx", "--region-dir", "d"];
        assert!(Cli::try_parse_from(dir).is_ok());
        assert!(
            Cli::try_parse_from(&ok[..4]).is_err(),
            "--region or --region-dir is required"
        );
    }

    #[test]
    fn requires_exactly_one_extent() {
        let base = ["ates", "area", "--dem", "d.tif", "--out", "o.tif"];
        assert!(Cli::try_parse_from(base).is_err());
        let both = [
            &base[..],
            &["--center", "-116.5,51.7", "--bbox", "-117,51,-116,52"],
        ]
        .concat();
        assert!(Cli::try_parse_from(both).is_err());
        let one = [&base[..], &["--center", "-116.5,51.7"]].concat();
        assert!(Cli::try_parse_from(one).is_ok());
    }
}
