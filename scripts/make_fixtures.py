"""Regenerate gdaldem reference outputs for the golden terrain tests.

Run with a Python that has GDAL's `osgeo` bindings, e.g. on Windows:

    %LOCALAPPDATA%\\Programs\\OSGeo4W\\bin\\python-qgis.bat scripts/make_fixtures.py

`gdal.DEMProcessing` is the library behind the `gdaldem` command, so this is
equivalent to `gdaldem slope|aspect dem.tif out.tif [-compute_edges]`.
"""

from pathlib import Path

from osgeo import gdal

gdal.UseExceptions()

FIXTURES = Path(__file__).resolve().parent.parent / "tests" / "fixtures" / "bow_summit"
DEM = FIXTURES / "dem.tif"
CREATION = ["COMPRESS=DEFLATE", "PREDICTOR=3"]


def main() -> None:
    info = gdal.Info(str(DEM), format="json")
    print("DEM:", DEM.name, info["size"], info["geoTransform"])
    print("CRS:", info["coordinateSystem"]["wkt"].splitlines()[0])
    print("nodata:", info["bands"][0].get("noDataValue"), "GDAL", gdal.__version__)

    for mode in ("slope", "aspect"):
        for edges, suffix in ((False, ""), (True, "_edges")):
            out = FIXTURES / f"{mode}_gdaldem{suffix}.tif"
            gdal.DEMProcessing(
                str(out),
                str(DEM),
                mode,
                format="GTiff",
                computeEdges=edges,
                creationOptions=CREATION,
            )
            print("wrote", out.name, out.stat().st_size, "bytes")


if __name__ == "__main__":
    main()
