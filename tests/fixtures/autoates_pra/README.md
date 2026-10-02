# AutoATES v2.0 PRA oracle fixtures (clipped)

The source files come from
[AutoATES/AutoATES-v2.0](https://github.com/AutoATES/AutoATES-v2.0) at commit
`3afcb49ae8c8a2385666f6fb4b999c69c82b1e83` (GPL-3.0), folder `PRA/`. That
folder holds the inputs and outputs of one run of `PRA_AutoATES-v2.0.py`.

The run is recorded in `PRA/log.txt`:
- forest type `stems`;
- radius 6 cells; prob 0.5; winddir 0; windtol 180; pra_thd 0.15; sf 3;
- Cauchy functions: slope 11/4/43, windshelter 3/10/3, forest (stems) 350/2/-120.

The originals are 801 × 801 cells at 10 m (ETRS89 / UTM 33N). The files here
keep the top-left **480 rows × 128 columns**, so the raster's real top and
left edges are included. They were cut with:
- `gdal_translate -srcwin 0 0 128 480 -co COMPRESS=DEFLATE`;
- `-co PREDICTOR=3` for Float32 files and `-co PREDICTOR=2` for Int16 files.

Pixel values, georeferencing and nodata are unchanged. Only the extent and
the compression differ.

`FOREST.tif` is offset from `DEM.tif` by (−3.5 m, −2.3 m). AutoATES ignores
georeferencing and pairs cells by index; the test does the same.

## Files

| File | What | SHA-256 of the unclipped original |
|---|---|---|
| `DEM.tif` | input DEM, Float32 | `a56835ce1ae2c90e3380d06e79e0a19ce22444204462acc3adeaa0a153ebfa18` |
| `FOREST.tif` | input stems per hectare, Float32 | `37de95847d735235d2d261d4a614bc37537f4f5385dcf06bdd429419e7ac288b` |
| `windshelter.tif` | windshelter index (radians) | `5479f9cf4aaa6b498b86272861c4a8d2c5cf72296e35de833f89f0b099d340cf` |
| `PRA_continous.tif` | PRA likelihood 0-100 (upstream spelling) | `f9d13300d18ef36ac4f034b8ffa71a5fcabaaf81f078af0a0be2e7a9802e3aa3` |
| `PRA_binary.tif` | binary PRA after the sieve | `35813925b6d6c49ec7e39c523c79f6e66c6abc149a39b5f7a9ee8bc1831c3364` |

At the same commit, `PRA/PRA_AutoATES-v2.0.py` has SHA-256
`c4d2755d1828d967df3e4c2d453b6f77de060ce9514190d748b26242cad5b12a` and
`PRA/log.txt` has `423787520ff08c1be67ec9cd105a3d599e3a4eb6fc37815f40eabd22943b7728`.

## What the test compares

`crates/ates-io/tests/pra_golden.rs` runs `ates_core::pra::pra` on the
clipped inputs. Near the clip's bottom and right edges, the windshelter
window, the slope and the sieve see different neighbours from the full
run, so the last 24 rows and columns are not compared. That leaves
456 × 104 = 47,424 cells.

The compared region includes:
- 456 DEM nodata cells (the raster's first column);
- all 5,859 cells with elevation 0, which the windshelter ignores;
- 28,231 forest-nodata cells and 18,960 cells with forest;
- 4,420 release cells;
- 105 cells changed by the sieve.

## Measured agreement (2026-10-02)

| Run | Cells compared | windshelter | PRA_continous | PRA_binary |
|---|---|---|---|---|
| Clipped fixtures | 47,424 | 0 differing | 0 differing | 0 differing |
| Full originals (ignored test, `ATES_PRA_ORACLE_DIR`) | 641,601 | 0 differing | 0 differing | 0 differing |

The full run includes 2,119 cells changed by the sieve.
`ates pra` on the full originals, with the forest given the DEM's
georeferencing, also matches all three rasters exactly. It takes about 4 s;
AutoATES's log records about 2 minutes.
