# ATES estimator — architecture and decisions

This is the starting point for future work sessions. It records what was agreed and why.
Update it when a decision changes.

## Goal

Given a coordinate, polygon or route, estimate Avalanche Terrain Exposure Scale classes:
- 0: non-avalanche terrain
- 1: simple
- 2: challenging
- 3: complex
- 4: extreme

The reference method is **AutoATES v2.0** (Toft et al., 2024, NHESS):
1. A potential release area (PRA) model.
2. Flow-Py runout.
3. Forest density applied in the PRA, in runout, and in post-classification.
4. Rule-based classification.

> Every output is a **modeled terrain classification, not an avalanche forecast.** This text is stamped into
> output metadata (`ates_io::DISCLAIMER`).

## Pipeline

```
Query (point | polygon | route)
  -> Prep: DEM (+ forest) window, warped to local UTM, padded by max runout
  -> Terrain: slope, aspect (later curvature, roughness)       ates-core::terrain
  -> Release areas (PRA) + forest filter                        ates-core::pra (+ ::sieve)
  -> Runout (Flow-Py with detrainment, rayon)                   ates-core::flowpy
  -> Overhead exposure (cell counts + z_delta, 0-100)           ates-core::overhead
  -> Classify (Classifier trait)                                ates-core::classify / ::autoates
  -> Trim pad -> Output (point answer, GeoTIFF, GeoJSON)
```

Design rules:
- **One code path.** Every query type reduces to the same steps: request extent → padded window → compute on the whole window → trim the pad → sample. Today this is `ates_pipeline::terrain`, which `point` and `area` both use. Classification runs on the full window before trimming, so smoothing and cluster steps see real neighbours.
- **Tiling.** Large areas will use overlapping tiles with a halo, so release areas outside a tile still contribute. That arrives in M6; until then a single window is capped at `MAX_WINDOW_CELLS` (100 M).
- **Projected CRS.** Reproject before computing slope. The analysis CRS is WGS 84 / UTM for the request centre (`ates_core::crs::utm_epsg_for`, regular 6° zones).
- **Config.** Thresholds live in TOML with named region presets. Every output stamps the tool version, config path and hash, region, and DEM source.
- **Swappable components.** `Classifier` is a trait (`ates_core::classify`), taking `TerrainLayers`. It has two implementations:
  - `SlopeBandClassifier`: proxy only.
  - `AutoAtesClassifier`.

  Runout is the function `ates_core::flowpy::flowpy`. A `Runout` trait waits until there is a second model to swap in. Raster I/O sits behind `RasterSource`/`RasterSink`/`Projector`. The GDAL-only cleanup step sits behind `FillNodata`, so `ates-core` stays I/O-free.

## Repository layout

```
Cargo.toml              workspace (resolver 3, edition 2024), shared deps and lints
rust-toolchain.toml     pinned 1.92.0 + rustfmt + clippy
config/default.toml     defaults + region presets (no unsourced science values)
crates/
  ates-core/            pure compute, no I/O: grid.rs, crs.rs, terrain.rs,
                        classify.rs (trait, slope bands), autoates.rs (AutoATES rules),
                        pra.rs (AutoATES PRA), sieve.rs (GDALSieveFilter port),
                        flowpy.rs (Flow-Py runout, rayon), overhead.rs,
                        route.rs (route evaluation), area.rs, tiles.rs (web tiles)
  ates-io/              raster traits, GDAL backend (feature "gdal"), provenance
  ates-pipeline/        config loading/merging, orchestration, comparison helper
  ates-cli/             `ates` binary (clap)
  ates-api/             `ates-api` HTTP server (axum) over region builds
web/                    map frontend: TypeScript + Vite + MapLibre GL + pmtiles
tests/fixtures/         small DEM clips + reference outputs (see their READMEs);
                        bow_summit/autoates/ = AutoATES inputs and every intermediate output
                        autoates_pra/ = clipped AutoATES PRA inputs and outputs
                        flowpy/ = Flow-Py test inputs and three reference runs
                        bow_summit/autoates/osf_flowpy/ = the Flow-Py run behind
                        AutoATES's Bow Summit inputs (Sykes et al. 2023, OSF)
data/regions/<name>/    region builds (git-ignored; `ates build-region`)
scripts/make_fixtures.py  regenerates gdaldem references (needs osgeo Python)
scripts/flowpy_oracle.py  runs upstream Flow-Py unchanged to make reference runs
.github/workflows/ci.yml  fmt, clippy -D warnings, tests (Ubuntu, GDAL 3.8)
flow-py/                vendored partial Flow-Py (GPL-3.0); reading material only
api/                    empty Python placeholders; left untouched. The API is crates/ates-api.
```

## Decisions and deviations from the original proposal

| # | Decision | Why |
|---|---|---|
| 1 | Workspace at the **repo root**, not `ates/` | The repo is this project. |
| 2 | Removed the placeholder `terrain/` crate and its committed build artefacts, and added `.gitignore` | It was `cargo new` boilerplate and would have been a stray crate inside the root workspace. |
| 3 | **GPL-3.0-or-later** for all crates, with `LICENSE` at the root | Flow-Py and AutoATES are GPL-3.0, and we port their algorithms. |
| 4 | **Oracle = upstream AutoATES-v2.0 at commit `3afcb49ae8c8a2385666f6fb4b999c69c82b1e83`** (2023-10-31), not `flow-py/` | The local copy is incomplete: `raster_io`, `Simulation` and `Flow_GUI` are missing, and it is modified. |
| 5 | CLI commands are `point`, `area` (which replaced M1's `terrain`), `pra`, `flowpy`, `classify`, `build-region`, `build-tiles`, `sample`, `route` and `check-slope`. `route` comes with route evaluation. | `area` writes the same slope/aspect plus the proxy class band. |
| 6 | `ates-pipeline` currently holds config, provenance and the terrain path. Tiling and caching come in M6. Remote DEM fetching comes in M2 or M3. | Keeps M1 small. |
| 7 | Config holds only **cited** scientific values. The AutoATES classifier defaults carry file and line citations. `prep.pad_m` is still TODO, and the CLI must supply it. | Working agreement: values come from the paper or the reference repo, with a citation. |
| 8 | Slope and aspect replicate gdaldem exactly, including `-compute_edges` extrapolation | It makes "match gdaldem" a strict, testable property. |
| 9 | CI runs on Ubuntu only | `apt install libgdal-dev` is trivial there. Windows is the documented local setup. |
| 10 | No `anyhow`; errors are `thiserror` enums, with `Box<dyn Error>` only in `main` | `anyhow` is not on the candidate dependency list. |
| 11 | `gdal` is an optional, default-off feature of `ates-io`. The CLI turns it on. | Keeps a pure-Rust backend possible and lets core, io and pipeline build and test without GDAL. |
| 12 | Resampling: **bilinear only** | The `gdal` crate's `reproject` is fixed to bilinear, and adding other modes would need `unsafe` FFI. Workspace lint is `unsafe_code = "deny"`. It is `deny` rather than `forbid` because ndarray's `s!` macro emits `allow(unsafe_code)`. |
| 13 | NaN is always treated as nodata, in addition to the declared nodata value | Safer than gdaldem, which treats NaN as nodata only when nodata is NaN. |
| 14 | **One `unsafe` exception:** `ates_io::gdal_backend::gdal_fill_nodata` calls `gdal_sys::GDALFillNodata` directly. `gdal-sys` is a direct, optional dependency. | The safe `gdal` crate doesn't wrap FillNodata, and AutoATES's cleanup is exactly this call. A float band truncated afterwards gave 147 mismatches; an Int16 band gives 0. Approved with the Increment 1–2 plan. A pure-Rust port can replace it. |
| 15 | The AutoATES port is **faithful, quirks included**, and every quirk is documented in `autoates.rs`. | It makes "matches the oracle" a strict, testable property. Deviating would be a deliberate, separately tested change. |
| 16 | Two output modes. `Product` (the default) keeps class 0 and masks cells where the DEM or forest is nodata. `OracleParity` reproduces `ates_gen.tif`, which writes class 0 as nodata. | AutoATES's output can't tell class 0 apart from no data. |
| 17 | Routing, CAIC data and the HTTP API wait until real ATES classes exist | Your decision (round 2): "real ATES first". |
| 18 | `gdal.SieveFilter` is **ported to Rust** (`ates_core::sieve`) instead of called through FFI | It keeps `ates-core` I/O-free and avoids a second `unsafe` exception. GDAL's algorithm (MIT) is ported step by step, including tie-breaking by scan order. It matches AutoATES's `PRA_binary.tif` exactly. |
| 19 | `pra` rejects non-square cells and forest rasters not on the DEM's grid | AutoATES uses the pixel width for both axes and pairs forest cells by index, ignoring georeferencing (its own test forest is offset by 3.5 m). The pipeline's warped windows are always square and aligned. |
| 20 | **`rayon`** is a dependency of `ates-core`, for Flow-Py | Planned in the approved Increment 1–2 plan ("a rayon port"). Paths run in parallel in batches of 256 release cells, and are folded in upstream's order, so results are deterministic and bit-identical to the sequential Python. |
| 21 | Flow-Py reference runs are **generated by us**, by running upstream code unchanged (`scripts/flowpy_oracle.py`) | AutoATES ships Flow-Py inputs but no outputs. Only I/O is swapped (GDAL for rasterio), and the multiprocessing split is dropped, which cannot change results without infrastructure. |
| 22 | Flow-Py defaults come from the **Sykes et al. (2023) Bow Summit run** (OSF): alpha 24, exponent 8, flux 0.003, max_z 270 | The paper publishes no Flow-Py values. That run's log is the only documented AutoATES Flow-Py configuration, and its outputs are AutoATES's own Bow Summit test inputs. Your decision (Increment 5): use them, but validate them for Colorado. |
| 23 | The overhead formula is **reconstructed** from data (`ates_core::overhead`) | The paper describes it only in words, and no code is published. The reconstruction reproduces the run's `Overhead.tif` on every cell. |
| 24 | Cameron Pass uses canopy-cover thresholds **20/55/75 from the paper** (Table 2), not the code's 10/50/65 | Your decision (Increment 5), marked TODO for you to research. |
| 25 | Flow-Py's forest layer at Cameron Pass is **canopy cover / 100** | Your decision. The run on OSF used an undocumented `forest_scaled.tif`. |
| 26 | Region builds use **one padded window with an adaptive pad**; tiling is deferred | Cameron Pass is 1.9 M cells and builds in about 1 minute. The pad is widened until it exceeds the longest modelled runout, instead of guessing `pad_m`. |
| 27 | Forest is fetched **on the DEM's grid** from the USFS ArcGIS ImageServer (`exportImage`, server-side nearest-neighbour), through `/vsicurl_streaming/` | No local warp, so there is no bilinear smoothing of a percentage. No new dependencies. The server does not support HTTP range requests, so `/vsicurl/` fails. |
| 28 | **`serde_json`** is a dependency, for reading GeoJSON routes and writing reports (the API will need it too). GPX is read by a small hand-written reader instead of the `gpx` crate. | Your decision (Increment 6). |
| 29 | Route evaluation is **evaluate-only** and reports modelled terrain facts: length per class, release areas, avalanche paths, overhead, aspect, elevation. It does not score or rank routes. | Your decision (round 2). Wording rule: never "safe". |
| 30 | **`axum`, `tokio`, `tower-http`** for the HTTP API | Approved for Increment 7. |
| 31 | The API **loads region builds into memory at startup** and never computes ATES per request. Aspect and slope are precomputed once. | Cameron Pass is about 1.9 M cells per layer, so memory is modest. Route requests take about 5 ms instead of 400 ms. Tiled COG window reads can replace this for large regions (Increment 10). |
| 32 | **CORS allows any origin** (GET/POST, `content-type`) | So a local frontend dev server can call it. Tighten before any public deployment. |
| 33 | The map overlay is a **PMTiles archive of lossless WebP raster tiles** | Your choice (Increment 8). One static file that the browser reads with range requests, loading only the visible tiles; it scales to large regions. Cameron Pass is 93 tiles, 93 KB in total. |
| 34 | PMTiles and tile rendering are **written in-house**: `ates_io::pmtiles` (writer), `ates_pipeline::tiles` (nearest-neighbour rendering), and WebP through GDAL's driver | No new Rust dependencies. The archive is validated with the reference `pmtiles` JS library (`web/scripts/check-pmtiles.mjs`). Only a root directory is written, which holds a few thousand tiles; larger pyramids need leaf directories. |
| 35 | Frontend: **plain TypeScript + Vite + MapLibre GL + pmtiles** in `web/`, with the USGS Topo basemap (public domain) | Your choice (Increment 8). In development, Vite proxies `/v1`; in production, `ates-api --web-dir web/dist` serves it, so everything shares one origin. |
| 36 | Overlay colours: class 1 green, 2 blue, 3 black, 4 red, semi-transparent; class 0 transparent | Classes 1–3 follow the usual ATES map convention; red for class 4 is our choice. The legend lives in the PMTiles metadata, so the frontend never hard-codes it. |

## Terrain (Milestone 1)

`ates_core::terrain::{slope_deg, aspect_deg}` use Horn's 3×3 method, written to match GDAL's `gdaldem_lib.cpp` exactly:
- The window is summed in `f32`, in gdaldem's exact order, before widening to `f64`. This matches `GDALSlopeHornAlg` and `GDALAspectAlg`, and the azimuth conversion is also done in `f32`. On non-integer (warped) DEMs it matters: summing in f64 gave aspect differences of up to 0.18° on near-flat cells.
- Slope uses the separate EW and NS resolutions.
- Aspect is an azimuth (0 = north, clockwise) and ignores pixel size, like gdaldem.
- Flat cells get aspect −9999. Output nodata is −9999.
- Without `compute_edges`:
  - The border ring is nodata.
  - Any nodata in a cell's window makes that cell nodata.
- With `compute_edges`:
  - A missing outer row or column is extrapolated as `2a − b`.
  - On the first and last rows, out-of-grid columns are clamped.
  - Any remaining nodata neighbour takes the centre value.

**Validation:** compared on the Bow Summit DEM (218×242 cells, non-square ~25.7×25.8 m pixels, ~30k valid cells), against GDAL 3.12.1:

| Mode | Slope max abs diff | Aspect max abs diff |
|---|---|---|
| default | 0 | 3.1e-5° |
| `-compute_edges` | 0 | 3.1e-5° |

On a bilinear-warped 30 m window (161×143 cells, about 16.9k compared) measured with GDAL 3.13.3 via `ates check-slope`: slope max diff 7.6e-6°, aspect max diff 3.1e-5°.

Neither mode had any validity mismatches. The tests use a 1e-3° tolerance:
- `crates/ates-io/tests/gdal_golden.rs` checks against the committed references and against in-process `gdaldem` on a window warped to UTM.
- `ates check-slope` runs the same comparison on any DEM.

## Configuration

`config/default.toml` contains `schema_version = 1`, a `[defaults]` table, and `[regions.<name>]` presets.
- A preset is deep-merged over the defaults.
- Unknown keys are rejected at load time, so typos fail fast. Every preset is validated when the file loads.
- A missing value that a run needs raises `ConfigError::Todo("<key>")`.
- **Rule:** every scientific value must cite its source in a comment: paper section, or repo file plus commit.

The AutoATES classifier defaults live in `[defaults.classify]` and `[defaults.classify.forest]`, each with a line citation to `AutoATES_classifier.py` at commit `3afcb49`.
- **They were verified against the file at that commit, and they reproduce the Bow Summit outputs cell for cell.**
- They are the AutoATES authors' defaults, **not** values tuned for any US region.
- `forest_type` is set per region: `bow_summit = "bav"`, `cameron_pass = "pcc"`.

Flow-Py defaults in `main.py` (alpha 25, exponent 8, flux 0.003, max_z 8848; the example run uses alpha 23 and max_z 270) are **GUI defaults and examples, not AutoATES calibrations**.

`[defaults.flowpy]` instead cites the Flow-Py log of the Sykes et al. (2023) Bow Summit validation run (OSF): alpha 24, exponent 8, flux 0.003, max_z 270. That run produced the `FP_int16.tif` and `Overhead.tif` in AutoATES's own Bow Summit test data. It was tuned for the Canadian Rockies; validating it for Colorado is an open TODO.

## Classification (Increments 1–2)

**Slope bands** (`ates_core::classify::slope_classes`) port `AutoATES_classifier.py` lines 99–124:
- Slope is cast to an integer by truncation.
- Nodata slope is set to 0 before smoothing.
- The smoothing reproduces `scipy.ndimage.uniform_filter(size=3, mode='nearest')` on int16. That means a separable mean, **rows (axis 0) first, truncated after each pass**. This was established empirically: it gives 0 mismatches, while the other orders and rounding modes give 3,398–19,408.
- Class 4 is assigned where smoothed slope > SAT34, including on cells whose own slope is nodata.
- On its own, this is the **slope-band proxy**: not ATES.

**AutoATES rules** (`ates_core::autoates`) port lines 132–350. Each function reproduces one intermediate raster:

```
flowpy_classes   FP travel angle -> 1..3 (AAT2, AAT3; AAT1 unused upstream)
cellcount_classes cell counts -> 1..3 (nodata -> 0 -> 1)
forest_codes     forest -> -1/10/20/30/40 (max of the four bands)
pra_codes        0/1 -> 0/100
merge_max        max(slope, flowpy, cellcount)
combine_lookup   merge + forest + pra -> class via the 40-entry table;
                 unlisted sums pass through; negatives -> 0
cleanup          8-connected same-value clusters < round(ISL/cell area)
                 -> refilled by GDALFillNodata (search num_cells/4 px)
```

**Validation:** on Bow Summit, with AutoATES's own `FP_int16`, `Overhead`, `forest` and `pra_binary` inputs, all 52,756 cells were compared:
- Every intermediate: **0 differing cells.**
- The final `ates_gen.tif`: **0 differing cells.**

See `tests/fixtures/bow_summit/autoates/README.md`.

`crates/ates-io/tests/autoates_golden.rs` passes with the real Rust `GdalFill` wrapper (GDAL 3.13.3). `ates classify --oracle-parity` on Bow Summit produces class counts identical to `ates_gen.tif`: `[0, 17334, 3307, 8347, 2194]`, nodata 21574. Product mode additionally masks 92 cells where the DEM itself has no data; AutoATES reports those as class 1.

**Hybrid pipeline (available now):**

```
ates classify --dem --forest --flowpy-fp --cell-count --pra --out [--region|--forest-type] [--oracle-parity] [--intermediates DIR]
```

The PRA can come from Python or from `ates pra`, and the Flow-Py rasters from Python or from `ates flowpy`. All inputs must already be on the DEM's grid. The whole chain now runs in Rust:

```
ates pra     --dem D --forest F --forest-type T --out pra.tif
ates flowpy  --dem D --release pra.tif [--forest FSI] --alpha A --flux-threshold X --max-z Z --out-dir fp/
ates classify --dem D --forest F --forest-type T --flowpy-fp fp/FP_travel_angle.tif               --cell-count fp/cell_counts.tif --pra pra.tif --out ates.tif
```

Smoke run on AutoATES's 801 × 801 PRA test DEM, using the GUI defaults:
- `ates pra` produced 90,556 release cells.
- Flow-Py took 21.9 s.
- `classify` completed.

This shows the chain works. It is not a validated map, because the Flow-Py parameters are uncalibrated. AutoATES used Flow-Py's output as `FP_int16.tif` and `Overhead.tif`. How it derived those files from `FP_travel_angle` and `cell_counts` is not in the repository. The classifier truncates to int16 anyway.

## Potential release areas (Increment 3)

`ates_core::pra` ports `PRA/PRA_AutoATES-v2.0.py` at `3afcb49`, the fuzzy-logic model of Veitinger et al. (2016) and Sharp (2018):

```
gradient_slope_deg  np.gradient slope (not Horn); cells < -100 -> -9999 first
windshelter         per cell: prob-quantile of atan((z - z0) / dist) over a
                    sector of radius r (full disc by default); z = 0 or nodata
                    ignored; -9999 border of r cells
memberships         Cauchy 1/(1+((x-c)/a)^(2b)) for slope, windshelter, forest;
                    forest <= 1e-5 -> 1; each rounded to 5 decimals
fuzzy_pra           m = min of the three; PRA = (1-m)m + m(sum)/3; round 5; x100
                    -> continuous (int16); binary: < thd -> 0, > thd -> 1
sieve_filter        GDALSieveFilter, threshold sf + 1, 8-connected
```

The script's numeric types are reproduced: on the float32 inputs AutoATES ships, slope, memberships and the fuzzy operator run in `f32`. The windshelter runs in `f64` and is stored as `f32`. The module docs list the quirks kept on purpose. One example: a binary value exactly equal to the threshold keeps its continuous value.

Parameters live in `[defaults.pra]` and `[defaults.pra.forest_cauchy]`, each with a line citation:
- The windshelter radius is configured as 60 m and rounded to whole cells; that is 6 cells at 10 m, as in AutoATES's run.
- Without a forest raster, the `pcc` function is applied to a forest of 0, as upstream's `no_forest` mode does.
- Upstream fails for `bav` and `sen2cc`, because it never loads the forest raster for them. We apply their cited functions, but those results are unverified.

**Validation:** AutoATES's `PRA/` folder holds one `stems` run on an 801 × 801, 10 m DEM. Against it:
- On the full originals, all 641,601 cells: windshelter, `PRA_continous` and `PRA_binary` each have **0 differing cells**. 2,119 cells are changed by the sieve.
- The committed clip, `tests/fixtures/autoates_pra`, compares 47,424 cells. It covers DEM nodata, elevation-0 cells, forest nodata and 105 sieve changes. Result: **0 differing cells.**
- `ates pra` reproduces all three rasters exactly in about 4 s; AutoATES's log records about 2 minutes.

```
ates pra --dem DEM.tif [--forest F.tif --forest-type stems|pcc|bav|sen2ccc] --out pra.tif [--region R] [--intermediates DIR]
```

The output is an Int16 GeoTIFF:
- band 1 is the binary PRA, so it can go straight to `ates classify --pra`;
- band 2 is the 0–100 likelihood.

`--intermediates` adds `windshelter.tif` and the unsieved binary.

The Bow Summit DEM has non-square cells (25.74 × 25.79 m), so `ates pra` refuses it. AutoATES's own PRA script never ran there either, because Bow Summit uses `bav`.

## Runout: Flow-Py (Increment 4)

`ates_core::flowpy` ports AutoATES's `FlowPy_detrainment`, at `3afcb49`:
- `flow_class.py`: the per-cell routing;
- `flow_core.py`: `calculation_effect`, the run without infrastructure.

For each release cell, a path spreads flux with flux 1 at the start. Each step goes through:

```
z_delta     energy-line height to each neighbour: z_delta + dz - ds*tan(alpha + forest friction),
            clipped to [0, max_z]
persistence previous flow direction, weighted by the parents' z_delta (0.707 to the sides)
routing     Holmgren: tan(beta/2)^exp over neighbours with z_delta > 0 and persistence > 0
detrainment forest removes flux (floor 0.0003)
distribute  flux below the threshold is pooled onto the receiving neighbours
travel angle atan(dh / shortest flow-path distance) from the release cell
```

The outputs, all per cell:
- maximum `z_delta`;
- maximum flux;
- `cell_counts`: the number of paths;
- `z_delta_sum`;
- maximum flow-path travel angle;
- minimum flow-path distance. Upstream calls this file `SL_travel_angle`.

**Bit-exact port.** Every arithmetic step follows what numpy 2 does on a float32 DEM:
- Elevation differences and persistence are `f32`; the rest is `f64`; outputs are `f32`.
- `np.sum` uses pairwise summation for 8 or more elements. Python's `max` and `min` keep the first argument on ties.
- Neighbours are sorted by `(z_delta, flux, row, col)`.
- A cell already processed can be re-added to its own path.

On Windows, numpy's `tan`, `arctan` and `pow` match the C runtime, which Rust also uses. That was checked on 180,000 random values. Each quirk kept is documented in the module.

Not ported:
- the infrastructure back-calculation;
- the unused `sl_gamma`.

Forest input is Flow-Py's forest layer on a 0–1 scale. It is **not** the classifier's density raster (pcc, bav, stems).

**Validation:** there are three reference runs of upstream code on its `examples/test` data (see `tests/fixtures/flowpy/README.md`). Each compares six outputs on all 30,012 cells, bit for bit (`f32::to_bits`). Result: **0 differing cells in every output** of every run. The runs cover:
- 36 and 1,516 release cells;
- with and without forest;
- `max_z` 270 and 8848.

Together they cover up to 22,553 cells on paths. The Rust port takes about 1 s for all three; Python took about 9 minutes.

## Region builds (Increment 5)

```
ates build-region --region cameron_pass [--out-dir DIR] [--pad-m M] [--bbox w,s,e,n]
ates sample --raster data/regions/cameron_pass/ates.tif --center lon,lat
```

`ates_pipeline::region::build_region` runs the whole chain on one padded UTM window:

```
3DEP DEM (/vsicurl/ COG) ─► warp to UTM 10 m, padded
USFS canopy cover (ImageServer, same grid) ─┐
                         PRA ─► Flow-Py (forest = pcc/100) ─► overhead ─► classify (Product) ─► trim
```

**Pad.** `prep.pad_m` is still unsourced, so the build starts at 1 000 m or `--pad-m`. After each run it finds the longest modelled flow-path distance in the window. While that distance is not shorter than the pad, it widens the pad to 1.5 × the runout and runs again, up to 10 km. The module doc explains why this is a heuristic.

**Outputs** in `data/regions/<name>/` are Cloud-Optimized GeoTIFFs with provenance and the disclaimer:
- `ates.tif` (classes 0–4)
- `pra.tif` (binary and continuous)
- `overhead.tif`
- `fp_travel_angle.tif`, `cell_counts.tif`, `z_delta.tif`
- `dem.tif`, `forest.tif`
- `manifest.toml`: every parameter used, the data sources, the pad attempts, class counts and timings.

**Cameron Pass, 2026-10-02:**

| | |
|---|---|
| Region | 1 456 × 1 286 cells at 10 m, UTM 13N |
| Release cells in window | 344 923 |
| Pad | 1 000 m (runout 1 315 m, too short) → 2 000 m (runout 1 315 m, sufficient) |
| Cells per class 0–4 | 0 / 1 424 706 / 251 787 / 174 748 / 21 175 (76 % / 13 % / 9 % / 1 %) |
| Time | 97 s from 1 000 m; 56 s with `--pad-m 2000` |
| Final attempt | DEM 6 s, canopy 3 s, PRA 14 s, Flow-Py 28 s, classify 1 s |

The build is deterministic: rebuilding gives identical class counts.

Class 0 never appears. That matches AutoATES, which uses no class-0 runout (AAT1 is unused) and maps cell-count nodata to class 1. Bow Summit's `ates_gen` has no class 0 either.

Spot checks (sanity only, not validation):
- **CO-14 at Cameron Pass:** class 1. The modelled elevation is 3 135 m, against about 3 132 m for the real pass.
- **Nokhu Crags:** class 3, inside a release area, travel angle 32°.

**This is not yet a validated map.** All parameters are authors' defaults or your interim choices; see the TODOs. The next step is to compare it with an expert ATES map, or with known avalanche paths.

## Route evaluation (Increment 6)

```
ates route --file route.gpx|route.geojson --region cameron_pass [--out report.geojson]
```

The route evaluation chain:
1. **Read.** `ates_io::route_file` reads GPX track segments and routes, or GeoJSON `LineString`/`MultiLineString` (bare, or inside a `Feature`/`FeatureCollection`).
2. **Project.** `ates_pipeline::route` projects the route onto the region build's UTM grid. It uses one PROJ transformation per part (`Projector::lonlat_to_many`).
3. **Evaluate.** `ates_core::route::evaluate` cuts each segment into equal pieces of at most half a cell (5 m at Cameron Pass) and samples each layer at every piece's midpoint. Consecutive pieces with the same class form a *stretch*. Parts never merge.

The report:
- **Totals:** route length in each ATES class, on nodata, and outside the region; plus length inside modelled release areas (`pra` = 1) and on modelled avalanche paths (Flow-Py travel angle > 0).
- **Per stretch:** distance along the route, elevation range, length per aspect sector, dominant aspect, release-area and avalanche-path length, and maximum overhead exposure.
  - Aspect uses 8 compass sectors, the same as CAIC forecasts, so Increment 9 can match avalanche problems.
  - Aspect is computed from the region DEM (Horn, as in gdaldem).
- **Lengths** are horizontal map distances.

Outputs:
- The terminal summary lists the class 3–4 stretches.
- `--out` writes a GeoJSON `FeatureCollection` with one `LineString` per stretch, in WGS 84 at 6 decimals. Each stretch keeps only its boundaries and the route's own vertices.
- A `summary` member holds the totals, the ATES v2 class names (Statham et al. 2018), the disclaimer, and the region build's `manifest.toml`, so every report records the parameters behind it.

Demo on Cameron Pass: illustrative straight legs from the pass toward the Nokhu Crags area, 4.9 km. Evaluation took 0.95 s, including reading the region layers.

| Class | Length |
|---|---|
| 1 | 4.18 km (85 %) |
| 2 | 0.61 km (12.5 %) |
| 3 | 0.12 km (2.5 %; one W-facing stretch at 3 356–3 392 m) |

The route also has 294 m in release areas and 658 m on modelled avalanche paths.

## HTTP API (Increment 7)

```
ates-api --data-dir data/regions --bind 127.0.0.1:8080
```

| Method | Path | Input | Output |
|---|---|---|---|
| GET | `/v1/health` | | `{status, tool_version, regions}` |
| GET | `/v1/regions` | | per region: bbox, EPSG, size, cells per class, COG URLs, full manifest |
| GET | `/v1/point` | `lon`, `lat`, optional `region` | class and name, elevation, slope, aspect (degrees and sector), canopy %, in release area, on avalanche path, travel angle, overhead |
| POST | `/v1/area` | GeoJSON Polygon/MultiPolygon (holes allowed); optional `?region=` | area and share per class, `nodata`, cells |
| POST | `/v1/route/evaluate` | GeoJSON line, or GPX (an `xml` content type, or a body starting with `<`); optional `?region=` | the Increment 6 GeoJSON report |
| GET | `/v1/files/{region}/{layer}.tif` | | Cloud-Optimized GeoTIFFs, with HTTP range requests, for a map client |

How it behaves:
- **Contract.** `crates/ates-api/src/types.rs` defines the response types the frontend relies on.
- **Disclaimer and provenance.** Every JSON response carries the disclaimer, and errors are JSON too (`{error, disclaimer}`). Provenance gives the tool version, the model, the region, and the build's config fingerprint.
- **Region choice.** Without `?region=`, the API picks the first region containing any vertex of the geometry, or the only region if there is just one.
- **Blocking work.** Route and area computations run on Tokio's blocking pool.

Tests:
- `crates/ates-api/tests/handlers.rs` calls each handler on a synthetic 20 × 20 region placed at Cameron Pass in UTM 13N, so projections are real. It covers point, outside (404), bad input (400), unknown region, area, and route from both GeoJSON and GPX.

Smoke test against the real Cameron Pass build, 2026-10-02:
- `/v1/regions` lists the build and its 8 COGs.
- `/v1/point` at Nokhu Crags: class 3, release area, travel angle 32°, aspect S.
- A point outside every region returns 404 with a JSON body.
- `/v1/route/evaluate` on the demo GPX matches `ates route` exactly, in about 5 ms.
- `/v1/area` on a box around the pass returns about 1.02 km², 99 % class 1.
- A `Range: bytes=0-1023` request on `ates.tif` returns 206.
- The CORS preflight from `localhost:5173` is allowed.

## Web map (Increment 8)

```
ates build-tiles --region cameron_pass [--min-zoom Z] [--max-zoom Z]   # also run by build-region
ates-api --data-dir data/regions --web-dir web/dist                     # http://127.0.0.1:8080
```

**Tiles.** `ates build-tiles` builds `ates.pmtiles` from `ates.tif` in three steps:
1. **Render.** `ates_pipeline::tiles` renders 256-pixel Web Mercator tiles in parallel. Each pixel takes the class of the grid cell under its centre: nearest neighbour, so classes are never blended.
2. **Encode.** Tiles are encoded as lossless WebP through GDAL. Empty tiles are skipped.
3. **Pack.** `ates_io::pmtiles` writes PMTiles v3. Identical tiles are stored once, and runs share an entry.

The zoom range is chosen automatically:
- **Highest zoom:** the first one whose pixels are no larger than the grid cell (z14 for 10 m at 40.5° N).
- **Lowest zoom:** four levels below that.

MapLibre overzooms beyond that with `raster-resampling: nearest`.

The archive metadata carries the legend (class names and RGBA), the attribution, the disclaimer, the tool version and the config fingerprint.

Cameron Pass, 2026-10-02:
- 93 tiles at z10–14, 93 KB in total (the raw RGBA would be 23.8 MB);
- rendered and encoded in 1.0 s.

The reference `pmtiles` library (v4.5) reads the header, the metadata and all 93 tiles as WebP (`npm run check-pmtiles`).

**Frontend** (`web/`, see `web/README.md`). A MapLibre map over the USGS Topo basemap, with the ATES overlay from PMTiles:
- an opacity slider, and a legend read from the tile metadata;
- click for a point popup (`/v1/point`);
- draw a route or upload GPX/GeoJSON to get the route panel (`/v1/route/evaluate`). The panel shows km per class, release-area and avalanche-path length, and a clickable list of class 3–4 stretches. The route line is drawn coloured by class.

The disclaimer is always visible, and repeated in popups and route results.

`src/api.ts` mirrors `ates-api`'s types. The production bundle is 289 KB gzipped JS, almost all of it MapLibre.

Checked:
- `tsc --noEmit` and `vite build` are clean, and `npm audit` reports 0 vulnerabilities.
- `ates-api --web-dir web/dist` serves the page and its bundle, plus range reads of `ates.pmtiles` (206).
- A brief manual browser check (2026-10-06): the overlay, point popups and route drawing work. That check found the fixes for MapLibre's worker, cache headers, layer order and fit padding.

Not yet checked: **GPX/GeoJSON upload** in the browser. The CLI and API paths are tested; the upload control is not.

## Provenance

GeoTIFF dataset metadata:
- `ATES_TOOL_VERSION`
- `ATES_PRODUCT`
- `ATES_CONFIG`
- `ATES_CONFIG_FNV1A64` (a fingerprint, not a cryptographic hash)
- `ATES_REGION`
- `ATES_DEM_SOURCE`
- `ATES_DISCLAIMER`

Band descriptions name each layer, e.g. `slope_class_proxy`, `slope_deg`, `aspect_deg` and `ates_class`. Bands are written as Int16 when every band is Int16 (class rasters); otherwise everything is written as Float32.

## Local setup (Windows)

GDAL comes from the OSGeo4W install at `%LOCALAPPDATA%\Programs\OSGeo4W`:
- `gdal` and `gdal-devel` **3.13.3**, which provide `include\gdal.h` and `lib\gdal_i.lib`;
- Arrow 25.

The **released** gdal crate (0.19.0 / gdal-sys 0.12.0) does not support GDAL 3.13: the C API renamed `GDT_Byte` and changed enum types. georust/gdal master does support it, with prebuilt 3.13 bindings, but is unreleased.

A git dependency can't be checked out on Windows: the GDAL submodule has paths longer than 260 characters. So this machine uses a **local, git-ignored override**:
- `.vendor/gdal-c0a6266…` holds the gdal and gdal-sys crates unpacked from GitHub's source archive of commit `c0a6266b987a2c2d44aea9956c2cae1ed71445cf`.
- `.cargo/config.toml` contains `[patch.crates-io]` pointing at them, plus `[env]` settings: `GDAL_HOME`, `GDAL_VERSION=3.13.3`, `GDAL_DATA`, `PROJ_DATA`.

CI does not see these files and keeps the crates.io releases with Ubuntu's GDAL 3.8. When gdal 0.20 is released:
1. Bump the dependency.
2. Delete `.cargo/config.toml` and `.vendor/`.

To recreate the override on another Windows machine:
```powershell
$r = "c0a6266b987a2c2d44aea9956c2cae1ed71445cf"
New-Item -ItemType Directory -Force .vendor | Out-Null
curl.exe -sSfL "https://codeload.github.com/georust/gdal/tar.gz/$r" -o .vendor\gdal.tgz
tar -xzf .vendor\gdal.tgz -C .vendor; Remove-Item .vendor\gdal.tgz
# then write .cargo\config.toml as described above (see the copy on the original machine)
```

To build and test, put the GDAL DLLs on the PATH and run cargo:
```powershell
$env:PATH = "$env:LOCALAPPDATA\Programs\OSGeo4W\bin;$env:PATH"
cargo test --workspace
```
When running `target\release\ates.exe` directly rather than through cargo, also set `GDAL_DATA` and `PROJ_DATA`, which `.cargo/config.toml` sets only for cargo. Without them, GDAL warns about missing data files, for example `tms_NZTM2000.json` when writing COGs.

No libclang is needed, because the prebuilt 3.13 bindings are used. LLVM is installed but unused.

**OSGeo4W troubleshooting.** Exit code −1073741515 (0xC0000135) means a DLL is missing. Two causes were found and fixed on this machine:
1. **Partial upgrade.** GDAL 3.13 was installed against Arrow 19, which has no `arrow_compute.dll`. Fix: run the installer with **Curr** to upgrade everything.
2. **`szip.dll` deleted.** The transitional `szip` package removed the file that `libaec` provides, so `hdf.dll` → `netcdf.dll` → `gdal313.dll` all failed to load. Fix: **Reinstall `libaec`**.

The symptom of either is that `gdalinfo`, QGIS's `crssync` post-install scripts (`qgis-common.bat`, `qgis-ltr-common.bat`) and QGIS Python's `osgeo` all fail.

To find the culprit, load every dependency with the real Windows loader. `ldd` on its own can mislead.

Without GDAL at all, on CI or another machine:
- `cargo test -p ates-core -p ates-io -p ates-pipeline --no-default-features` runs everything except the GDAL tests.
- `DOCS_RS=1 CARGO_TARGET_DIR=target/docsrs-check cargo clippy --workspace --all-targets` type-checks the GDAL code using prebuilt bindings. It does not link.

## Data

- **Golden region:** Bow Summit (from AutoATES test-data, UTM 11N).
- **First US region: Cameron Pass** (preset `cameron_pass`):
  - The bbox `[-105.95, 40.45, -105.80, 40.58]` is approximate and still to confirm.
  - DEM: USGS 3DEP 1/3″ seamless, tile `n41w106`, read over HTTP as a Cloud-Optimized GeoTIFF (`site.dem_path`). The analysis grid is UTM 13N at 10 m.
  - Forest: USFS Science Tree Canopy Cover, CONUS, v2025-6, year 2024 (`site.forest_service`, `forest_where = "beginyear=2024"`). Values 254 and 255 are nodata. The source is 30 m, resampled to 10 m by the server.

## Roadmap: zones, routes, CAIC, API

**Principle:** ATES is static terrain, but forecasts change daily. Keep them separate.
1. **Precompute** ATES per region or zone, offline.
   - Tiles overlap with a halo of at least `pad_m`, so release areas just outside a tile still contribute.
   - Results are stored as COGs under `data/regions/<name>/` and stamped with provenance.
2. **At query time**, sample the precomputed classes: a point, polygon statistics, or a route's exposure profile (metres per class, plus where the route crosses class 3–4).
3. **Overlay the forecast** at query time, as context only. Per route segment:
   - the CAIC zone,
   - the danger for its elevation band (`alp`/`tln`/`btl`),
   - whether its aspect and elevation fall in a listed avalanche problem.

   The forecast never silently re-weights a route.

| Stage | Where | Notes |
|---|---|---|
| Region presets | config | bbox, data sources, forest_type, CAIC zone, treeline elevations per zone (TODO) |
| DEM and forest providers | `ates-io` | 3DEP and TCC, fetched once, cached |
| Route evaluation | `ates-core::route` (pure) | Densify the polyline at cell spacing, sample classes, aspect and elevation; GeoJSON or GPX in. Evaluate-only first (your decision). |
| Forecasts | new `ates-forecast` | `ForecastProvider` trait plus a CAIC implementation; recorded fixtures |
| API | new `ates-api` (axum + tokio + serde_json + tower-http) | `POST /v1/point`, `/v1/area`, `/v1/route/evaluate`; `GET /v1/regions`, `/v1/forecast/{zone}`. Shared serde types are the frontend contract. Every response carries provenance and the disclaimer. |

**CAIC data access** (read-only probes, 2026-10-02):
- avalanche.org's public map layer has CAIC only as a single statewide "CAIC zone", with no per-area danger and no problems.
- CAIC's site uses an **undocumented** JSON proxy, `https://avalanche.state.co.us/api-proxy/avid?_api_proxy_uri=/products/all...`:
  - Products of type `avalancheforecast` carry `dangerRatings.days[].{alp,tln,btl}`, `avalancheProblems.days[]` and an `areaId`.
  - `/products/all/area` returns GeoJSON MultiPolygons keyed by id.
- **No published API or terms.** Ask CAIC for permission before relying on it.
  - Send an identifying User-Agent and cache responses.
  - Never present forecast data as our own.
- The problem schema (aspect/elevation codes) still needs a captured in-season sample. It was empty in October.

**Wording rule:** outputs say "lower modeled exposure" and "forecast context", never "safe".

## Open TODOs

- Drop the local gdal override (`.cargo/config.toml`, `.vendor/`) once gdal 0.20 is on crates.io.
- **Validate the Flow-Py `alpha_deg`, `flux_threshold` and `max_z_delta` for Colorado.** They are currently the Sykes et al. (2023) Bow Summit values. Compare runouts with known paths, for example from CAIC records or local experts. Sensitivity runs need only `ates build-region` with an edited preset.
- **(Owner) Research the canopy-cover thresholds.** Cameron Pass uses Toft et al. (2024) Table 2's 20/55/75, while the code has 10/50/65. Find out how each was derived.
- How the Sykes run scaled basal area into `forest_scaled.tif`. We use canopy cover / 100 for Cameron Pass.
- `prep.pad_m`: the region build measures runout instead. Cameron Pass needed 2 000 m (longest runout 1 315 m).
- Overhead exposure is normalised by the window's maximum cell count, as in AutoATES. So tiles or other extents would scale differently. Decide on a fixed reference before tiling.
- Cameron Pass: confirm the bbox, compare with an expert ATES map, and find the CAIC area id (in season).
- Bow Summit full-chain parity: that run's DEM was int16, but our Flow-Py port reproduces float32 arithmetic. Its input DEM and `forest_scaled.tif` are also not in the OSF archive.
- Regional tuning of AutoATES thresholds for Colorado. For now they are the authors' defaults. This includes the PRA parameters:
  - The wind direction and tolerance default to "all directions".
  - The `pcc` forest function has not been checked against USFS canopy cover.
- `bav` and `sen2ccc` PRA can't be verified against AutoATES, because the upstream script fails for both.
- Decide whether to add `tracing-subscriber`, which is off the candidate list.
- Treeline elevations per CAIC zone, needed for forecast context.
- Test GPX and GeoJSON upload in the web map, in a browser.

## Milestones and increments

1. **Scaffold, warp a DEM window, slope/aspect matching gdaldem** — done.
2. **Slope-band proxy** (`point`, `area`) — done (Increment 1). Golden-matched to AutoATES `slope.tif`.
3. **PRA** — done (Increment 3). `ates pra`; windshelter, continuous and binary PRA match AutoATES on every cell.
4. **Runout** — done (Increment 4). `ates flowpy`; all six outputs bit-identical to upstream Flow-Py on three reference runs.
5. **Classification rules** — done (Increment 2). Every intermediate and `ates_gen` match AutoATES.
6. **Cameron Pass end to end** — done (Increment 5). `ates build-region` produces Cloud-Optimized GeoTIFF ATES classes from 3DEP and USFS canopy cover in about 1 minute, and `ates sample` reads them at a point. Parameters are not yet validated for Colorado.
7. **Route evaluation** — done (Increment 6). `ates route` gives GPX or GeoJSON in, a terminal summary, and a GeoJSON report out.
8. **HTTP API** — done (Increment 7). `ates-api` serves regions, points, areas, route evaluation and the COGs.
9. **Frontend MVP** — done (Increment 8). PMTiles WebP overlay, point popups, route drawing and upload.
10. Next: CAIC forecast context (Increment 9) and scaling beyond one window (10).
