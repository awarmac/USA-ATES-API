# Flow-Py oracle fixtures

## Inputs

The inputs are copied unmodified from
[AutoATES/AutoATES-v2.0](https://github.com/AutoATES/AutoATES-v2.0) at commit
`3afcb49ae8c8a2385666f6fb4b999c69c82b1e83` (GPL-3.0), folder
`FlowPy_detrainment/examples/test/`. They are 246 × 122 cells at 10 m,
ETRS89 / UTM 33N, with nodata −3.4028235e+38 in the float rasters.

| File | What | SHA-256 |
|---|---|---|
| `dem.tif` | DEM, Float32 | `ca3ac0ca88211b33e2d27d1c36c86f7c937b52328376b7cc919ef85c5a274988` |
| `pra.tif` | release, 1516 cells of 1 | `02aac4512f7160aff9ab5f7f1fe230bc17f0a782f6f2800d80123560046ce1cb` |
| `pra2.tif` | release, 36 cells of 1 (Int16, rest −9999) | `c86abce9f33043e2b8001ae728a8e389b0d1c6d2f4c1233f5aead4c6270f342d` |
| `forest2.tif` | Flow-Py forest layer, 0–1 | `e059e97cda91ee430a39e71d817c579ce4f51db99f0d89b2dd916363edd3acd3` |

## Reference runs

AutoATES ships no Flow-Py outputs, so these were produced by running the
upstream code unchanged. The upstream files used, with their SHA-256:
- `flow_class.py` `8fba3f5ceead94fd51fcbb1ff4c2bcf4f3fca1c9834e7424af85583a685671d7`
- `flow_core.py` `dbcc2a9748fbe2081160501bceb754ddf7e92834d4ddd75d3285263a55acd0d4`
- `main.py` `c72af0d1058e80428fa51d022ac692834b513ea6e05f2f37f194a4fdb3d7590a`

Setup: Python 3.12 and numpy 2.4.6 from OSGeo4W, Windows 11, run on
2026-10-02.

The runs go through `scripts/flowpy_oracle.py`, which calls `flow_core.calculation_effect` (the run without infrastructure) and combines its outputs the way `main.py` does. Three things differ from running `main.py` itself:
- **Raster I/O** uses GDAL instead of rasterio, which isn't installed. It reads the same arrays and header fields.
- **One process instead of a pool.** Upstream splits the release cells by column across processes. Without infrastructure every path is independent, so this changes nothing except possibly the last bit of `z_delta_sum`, which depends on summation order.
- **Outputs are written** as DEFLATE-compressed Float32 GeoTIFFs with nodata −9999. Upstream also writes nodata −9999, uncompressed.

| Directory | Release | Forest | alpha | exp | flux | max_z | Python time |
|---|---|---|---|---|---|---|---|
| `pra2_forest/` | `pra2.tif` | `forest2.tif` | 23 | 8 | 0.003 | 270 | 5 s |
| `pra_forest/` | `pra.tif` | `forest2.tif` | 23 | 8 | 0.003 | 270 | 184 s |
| `pra_noforest/` | `pra.tif` | none | 25 | 8 | 0.003 | 8848 | 373 s |

The parameters come from upstream examples, not calibrations:
- 23 / 8 / 0.003 / 270 with `forest2.tif` are `main.py`'s `__main__` arguments. That example also passes `infra.tif`, which is left out here.
- 25 / 8 / 0.003 / 8848 are the GUI defaults.

Each directory has `run.txt` with the exact arguments, and the six outputs:
- `z_delta`, `flux`, `cell_counts`, `z_delta_sum`;
- `FP_travel_angle`;
- `SL_travel_angle`. Despite the name, this is the minimum flow-path distance.

| File | SHA-256 |
|---|---|
| `pra2_forest/FP_travel_angle.tif` | `d27c68691d86db0ac1049af6770b1f976cf3df6d9a758034974f329e41d7658d` |
| `pra2_forest/SL_travel_angle.tif` | `25c45f003ecebba8d9e9938ab9dd242429570806f46129c7447232f96064df8e` |
| `pra2_forest/cell_counts.tif` | `c35e51132059fb11c9d42db5ad193906fdf4a29251ade94558f3cd6116c7017d` |
| `pra2_forest/flux.tif` | `2c83bfc775c688b160a5aea35016432c8294dad49cbb1e1502f82d3a423550ce` |
| `pra2_forest/z_delta.tif` | `a52a17985d241a77d3b8f201194bb8c51fa4927c076b9cffbed420fa864123a0` |
| `pra2_forest/z_delta_sum.tif` | `8e9c24239ca89a9903ec4a923973a2717d5e32cda390a50a5e1ed0b041d99b34` |
| `pra_forest/FP_travel_angle.tif` | `38f2ea1ba7fc56e4c96c0de7ab9e5320b3d20a5bac516f907f53abbfb688d94a` |
| `pra_forest/SL_travel_angle.tif` | `41a1d90b8bbb0ce8fcbf4b0ed085ddf05d7af40c2fe4e6135d192d66ff888187` |
| `pra_forest/cell_counts.tif` | `63fca7d5af6fa65c26d0aca3c95fd017d6aa64b76953e1d92a06fce04eaf97d1` |
| `pra_forest/flux.tif` | `c960b412005d56620e5e7f5d2db11f58c5b72d6344dd15224c07a091d21b48fd` |
| `pra_forest/z_delta.tif` | `108f3d63f00e1a7d3729bf9977d90a7246992124a2edba4f6593d90941d659a4` |
| `pra_forest/z_delta_sum.tif` | `3f6a854617da56d503518bf6b538f62ad6debd5f58751e9e86c4c3f0eded8df0` |
| `pra_noforest/FP_travel_angle.tif` | `9c63de00947904fa804ead8b25a1a4071cd90ec38e6dc0658aa0c2f0f2fd23e1` |
| `pra_noforest/SL_travel_angle.tif` | `b58f28d1a6e30ed2df54ada0eefef77821e1f3513f2749b4aa3ca70dca094db3` |
| `pra_noforest/cell_counts.tif` | `f32f16551a982fac3f2c6f2bdf2b285dd39ed3e3ebbc819697f891d7bc42e6fb` |
| `pra_noforest/flux.tif` | `dab24680643237e0e797f2c058e8614f0402c5bfe16184572757e34a3390b3ab` |
| `pra_noforest/z_delta.tif` | `fa7c94eb2c0119f7d8b4b4df8472ba42f7d0f292487ee5b82364c9ea435eb2f5` |
| `pra_noforest/z_delta_sum.tif` | `f84c87de8274053e3b95cdcb7b87949684c731734cd0ce11866ade7a40e12040` |

## Measured agreement (2026-10-02)

`crates/ates-io/tests/flowpy_golden.rs` compares all six outputs of every run, bit for bit (`f32::to_bits`), over all 30,012 cells:

| Run | Cells on paths | Differing cells, each of the 6 outputs |
|---|---|---|
| `pra2_forest` | 3,623 | 0 |
| `pra_forest` | 17,290 | 0 |
| `pra_noforest` | 22,553 | 0 |

The Rust port runs all three in about 1 s, against about 9 minutes for Python.
