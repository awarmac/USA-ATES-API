# Bow Summit fixtures

Golden-test inputs and references for terrain derivatives.

| File | What | SHA-256 |
|---|---|---|
| `dem.tif` | DEM, 218 x 242 cells, ~25.7 x 25.8 m, WGS 84 / UTM 11N (EPSG:32611), nodata -9999 | `c46dca9b0625208502f763354bdff0ace17519487e3b78feef1cba480151bc2f` |
| `slope_gdaldem.tif` | `gdaldem slope` (Horn, degrees) | `66a0728831ef300e5fcd884ee15f65f99962501c8fd4544905d8b37904c61207` |
| `slope_gdaldem_edges.tif` | same, `-compute_edges` | `a74e5eb0a19eb960faa17dea1202425e4879e0258073e96911b0792837a31486` |
| `aspect_gdaldem.tif` | `gdaldem aspect` (Horn, azimuth) | `9e526eadf5199dc325ea26a348d969fca2288a652447d73c5cd9487c438ce574` |
| `aspect_gdaldem_edges.tif` | same, `-compute_edges` | `9cf7e4e70e93fa6b573cd8426c554200af0633f4d921ea347ef8cfa0610874ee` |

## Provenance

- `dem.tif` is copied unmodified from
  [AutoATES/AutoATES-v2.0](https://github.com/AutoATES/AutoATES-v2.0) at commit
  `3afcb49ae8c8a2385666f6fb4b999c69c82b1e83`, path `test-data/Bow Summit/dem.tif`
  (GPL-3.0). The same folder holds AutoATES's PRA, Flow-Py and forest rasters,
  which later milestones will use as oracle outputs.
- The `*_gdaldem*` files were generated with GDAL 3.12.1 (OSGeo4W) by
  `scripts/make_fixtures.py`. Regenerate them with:

  ```
  %LOCALAPPDATA%\Programs\OSGeo4W\bin\python-qgis.bat scripts\make_fixtures.py
  ```

## Measured agreement (ates-core vs. these files)

Measured during Milestone 1 on about 30k valid cells:

- Slope: max abs diff **0**, in both default and `-compute_edges` modes.
- Aspect: max abs diff **3.1e-5°**, in both modes.
- Validity mismatches: zero.

The tests allow 1e-3°.
