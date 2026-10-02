"""Run AutoATES's Flow-Py (FlowPy_detrainment @ 3afcb49) unchanged, in one
process, and write its outputs. Only raster I/O differs from main.py:
GDAL instead of rasterio, with the same arrays (native dtype, band 1) and
header fields (cellsize = transform[0], noDataValue = band nodata).

Run from a checkout of AutoATES-v2.0/FlowPy_detrainment (so `flow_core`
imports), with an osgeo + numpy Python such as OSGeo4W python-qgis.bat.

usage: flowpy_oracle.py OUT_DIR DEM RELEASE [FOREST|-] ALPHA EXP FLUX MAX_Z
"""
import os
import sys
import time

import numpy as np
from osgeo import gdal

import flow_core as fc

gdal.UseExceptions()


def read(path):
    ds = gdal.Open(path)
    b = ds.GetRasterBand(1)
    header = {"ncols": ds.RasterXSize, "nrows": ds.RasterYSize,
              "cellsize": ds.GetGeoTransform()[1], "noDataValue": b.GetNoDataValue()}
    return b.ReadAsArray(), header, ds


def write(ref_ds, path, arr):
    drv = gdal.GetDriverByName("GTiff")
    out = drv.Create(path, ref_ds.RasterXSize, ref_ds.RasterYSize, 1, gdal.GDT_Float32,
                     ["COMPRESS=DEFLATE", "PREDICTOR=3"])
    out.SetGeoTransform(ref_ds.GetGeoTransform())
    out.SetProjection(ref_ds.GetProjection())
    b = out.GetRasterBand(1)
    b.SetNoDataValue(-9999)
    assert arr.dtype == np.float32, arr.dtype
    b.WriteArray(arr)
    out = None


def main():
    out_dir, dem_p, rel_p, forest_p, alpha, exp, flux, max_z = sys.argv[1:9]
    os.makedirs(out_dir, exist_ok=True)
    dem, header, dem_ds = read(dem_p)
    release, rel_header, _ = read(rel_p)
    forest = read(forest_p)[0] if forest_p != "-" else np.zeros_like(dem)
    # split_release's preprocessing, with a single piece.
    release = fc.split_release(release, rel_header, 1)[0]
    t = time.time()
    res = fc.calculation_effect([dem, header, forest, release, alpha, exp, float(flux), max_z])
    print("calculation_effect took %.1f s" % (time.time() - t))
    z_delta, flux_a, counts, zsum, _backcalc, fp_ta, fp_dis = res
    # main.py's combination with its initial arrays.
    out = {
        "z_delta": np.maximum(np.zeros_like(dem), z_delta),
        "flux": np.maximum(np.zeros_like(dem), flux_a),
        "cell_counts": np.zeros_like(dem) + counts,
        "z_delta_sum": np.zeros_like(dem) + zsum,
        "FP_travel_angle": np.maximum(np.zeros_like(dem), fp_ta),
        "SL_travel_angle": np.minimum(np.ones_like(dem) * 10000, fp_dis),
    }
    for name, arr in out.items():
        write(dem_ds, os.path.join(out_dir, name + ".tif"), arr)
    with open(os.path.join(out_dir, "run.txt"), "w") as f:
        f.write("numpy %s\nargs %s\nrelease cells %d\n" % (np.__version__, sys.argv[1:], int((release > 0).sum())))


if __name__ == "__main__":
    main()
