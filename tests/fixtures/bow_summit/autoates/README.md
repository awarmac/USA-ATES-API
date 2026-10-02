# AutoATES v2.0 oracle fixtures (Bow Summit)

These files are copied unmodified from
[AutoATES/AutoATES-v2.0](https://github.com/AutoATES/AutoATES-v2.0) at commit
`3afcb49ae8c8a2385666f6fb4b999c69c82b1e83` (GPL-3.0), under `test-data/Bow Summit/`.
Files prefixed `out_` come from that folder's `outputs/` subfolder.

All rasters are Int16 with nodata −9999, on the same 218 × 242 grid as `../dem.tif` (UTM 11N).

The run parameters are in `out_inputpara.csv`:
- forest type `bav` (TREE 10/20/25),
- `cell_count = Overhead.tif`,
- SAT 15/18/28/39, AAT 18/24/33, CC 5/40, ISL_SIZE 30000, WIN_SIZE 3.

## Inputs

| File | What | SHA-256 |
|---|---|---|
| `FP_int16.tif` | Flow-Py flow-path travel angle | `c91ee6d0e7158333de3f2bab20e64899f06253e1334b8ef8a907b3b39462ed1a` |
| `Overhead.tif` | Flow-Py cell counts, used as `cell_count` | `2096dacbdde418f1b5a06485e60d6c03514b3f6c46616e1e0cf1fc3cc642d9d2` |
| `forest.tif` | basal area (`bav`) | `f9b279f6097a907cb37be0c3ff4416346961dbd62ceca8a4f29c81a3e66d72ab` |
| `pra_binary.tif` | potential release areas (0/1) | `06c290aaeddca5a75859c82b37561fe987582987f10b3b0161c82bbed7a7bbc9` |

## Reference outputs

| File | AutoATES step | SHA-256 |
|---|---|---|
| `out_slope.tif` | slope classes | `f7730a2312766d2d538953f17853fb5b69017b5a46fd3f1c4b819d7d09b21afc` |
| `out_slope_smooth.tif` | smoothed integer slope | `3f586520c53880a0ce8ceceb0f243581b4245bf9caf81a40875e42586ce0ce2e` |
| `out_flowpy.tif` | runout classes | `6c72670aac98ecb60e4127be7f6d408116fa8b2c6467590f6e86e2163a9834bd` |
| `out_cellcount_reclass.tif` | cell-count classes | `05220feeff701cd55635e4345bd72e42f22bc28244d2de3772eadd254a872483` |
| `out_forest_reclass.tif` | forest codes | `7b0effe7b52a1c3ebd13f55d6a8a76b841a01cb5b46896c723cc84ba6104ccbf` |
| `out_SZ_reclass.tif` | PRA codes | `b1acff7a842d384abff08b8d73bdbc7668203e629b9ffe45294de41e4cb81d38` |
| `out_merge_new.tif` | max merge | `7b8f01348d71862d65b471496b07c5a2f0d6051295f9fcc8e7f4f4700cbc0681` |
| `out_merge_all.tif` | forest/PRA lookup | `956f71ae1c1f0b779e273e1a8f75ef6df8bddeb5fc0d9171e5b37e63e2deb772` |
| `out_ates_gen.tif` | final ATES (class 0 written as −9999) | `d8d3dd7da451f1544cd42ef065bedccea9c2ba5d13d3049b41caf68aeebcbdc9` |
| `out_inputpara.csv` | parameters used | `5ee4a51cd3f7f96d3403b65f9618ecb4feb99136ea7b733626b1598e25a6e93b` |

## Measured agreement

These comparisons ran in the scratchpad during Increment 2, on raw dumps of these files over all 52,756 cells:

- `ates_core` slope classes, smoothed slope, flowpy, cell-count, forest, PRA, `merge_new` and `merge_all`: **0 differing cells** for every layer.
- `ates_gen`: **0 differing cells**, using our pre-fill raster and cleanup mask passed through GDAL `FillNodata` (QGIS Python, GDAL 3.12.1, Int16 MEM band).
  - Our cleanup mask flags 1,095 cells for refilling (45-cell minimum cluster).

`crates/ates-io/tests/autoates_golden.rs` runs the same checks through the Rust `GdalFill` wrapper. It passes against GDAL 3.13.3 (OSGeo4W, 2026-10-02) with 0 differing cells for every layer and for `ates_gen`.

## The Flow-Py run behind `FP_int16.tif` and `Overhead.tif` (`osf_flowpy/`)

AutoATES's Bow Summit inputs come from the Bow Summit validation run in:

> Sykes, J., Toft, H. B., and Haegeli, P.: Automated Avalanche Terrain
> Exposure Scale (ATES) mapping – Local validation and optimization in
> Western Canada, OSF [code], https://doi.org/10.17605/OSF.IO/ZXJW5, 2023
> (GPL-3.0).

In that archive, `Grid search localization/BowSummit_ALOS30m_final.zip` (SHA-256 `0fa694255f308911ed7fdfd02cd582499a046723afb1ff538652dce6d75c3f7c`) contains `ALOS30m_final/flowpy/`. That folder's `FP_int16.tif`, `Overhead.tif` and `PRA/pra_binary.tif` are identical, cell for cell, to the files here.

`osf_flowpy/` holds that run's raw Flow-Py output:
- the log, which records alpha 24, exponent 8, flux threshold 0.003 and max z_delta 270;
- three rasters from `res_20230425_170518/`, recompressed with DEFLATE. Values are unchanged.

| File | SHA-256 of the original |
|---|---|
| `cell_counts.tif` | `f6f2597adae9d17c1e6eee9bda2684fc8c4dd03e78989e99b8ad06d4afe0d9af` |
| `z_delta.tif` | `23e81c53ebb0c2872334a7b2edf8a9254131fbf179666f0915e9fdbf4825021c` |
| `FP_travel_angle.tif` | `884d9bed00c87c3c767e9ad9f875d7fdb142fc89151f8854557f55fb9eff5331` |
| `log_20230425_170518.txt` | `5fd3ad6798dc3ff230a9e08c1f046959e2886e77862dc97c482f4f73c5f01d12` (unchanged) |

`autoates_golden.rs::overhead_and_fp_inputs_match_their_flowpy_run` checks that both files can be derived from these rasters. It matches on all 52,756 cells:
- `FP_int16.tif` is the travel angle truncated to int16.
- `Overhead.tif` is `trunc((100 ln(cell_counts) / ln(max cell_counts) + 100 z_delta / 270) / 2)` (`ates_core::overhead`).
