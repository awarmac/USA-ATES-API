# ATES web map

A MapLibre map of **modeled** ATES terrain classes over the USGS topo
basemap. It is not an avalanche forecast.

You can:
- click anywhere for terrain details (`/v1/point`);
- draw a route or upload a GPX/GeoJSON file for a route evaluation (`/v1/route/evaluate`).
  When `ates-api` runs with saved forecast files (`--caic-products`, `--caic-areas`), the route
  panel also shows forecast context: danger and listed problems per stretch. It never changes a class.

The stack is plain TypeScript, Vite, MapLibre GL and the `pmtiles` library. The ATES overlay is a PMTiles archive of lossless WebP tiles, written by `ates build-tiles` (or `ates build-region`) and served by `ates-api` under `/v1/files/<region>/ates.pmtiles`.

## Develop

Start the API, then the Vite dev server (it proxies `/v1` to the API):

```powershell
# repository root, with OSGeo4W on PATH
cargo run --release -p ates-api -- --data-dir data/regions --bind 127.0.0.1:8080

# in web/
npm install
npm run dev        # http://localhost:5173
```

Set `ATES_API` to proxy to another API address. Set `VITE_API_URL` to call
an API on another origin directly; the API allows CORS, including range
requests.

## Build and serve from the API

```powershell
npm run build      # type-check + bundle into web/dist
cargo run --release -p ates-api -- --web-dir web/dist   # http://127.0.0.1:8080
```

## Check a tile archive

```powershell
npm run check-pmtiles -- ../data/regions/cameron_pass/ates.pmtiles
```

This reads the header, metadata and every tile with the reference `pmtiles` library.

## Notes

- **API types:** `src/api.ts` mirrors `crates/ates-api/src/types.rs`; keep them in sync.
- **Basemap:** USGS The National Map, USGSTopo (public domain). It needs internet access.
- **Wording:** the UI never calls terrain or a route "safe". The disclaimer is always visible and repeated in popups and route results.
