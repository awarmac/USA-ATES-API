// ATES map: modeled avalanche terrain classes over a USGS topo basemap,
// with point details and route evaluation from ates-api.
//
// Wording rule: this shows modeled terrain only. It never calls terrain or
// a route "safe" and is not an avalanche forecast.

import * as maplibregl from "maplibre-gl";
import type { GeoJSONSource, MapMouseEvent } from "maplibre-gl";
import "maplibre-gl/dist/maplibre-gl.css";
// MapLibre v6 runs sources such as GeoJSON in a module worker that it
// looks for next to its own file; bundlers do not copy it there, so the
// worker failed to load and drawn routes never rendered. Let Vite bundle
// the worker and tell MapLibre where it is.
import maplibreWorkerUrl from "maplibre-gl/dist/maplibre-gl-worker.mjs?worker&url";
import { PMTiles, Protocol } from "pmtiles";
import "./style.css";
import {
  apiUrl,
  evaluateRoute,
  getPoint,
  getRegions,
  type PointResponse,
  type ForecastSummary,
  type RegionInfo,
  type RouteReport,
  type StretchForecast,
} from "./api";

interface LegendEntry {
  class: number;
  name: string;
  rgba: [number, number, number, number];
}

const $ = <T extends HTMLElement>(id: string) => document.getElementById(id) as T;

maplibregl.setWorkerUrl(maplibreWorkerUrl);

const protocol = new Protocol();
maplibregl.addProtocol("pmtiles", protocol.tile);

const map = new maplibregl.Map({
  container: "map",
  center: [-105.875, 40.515],
  zoom: 12,
  maxZoom: 17,
  style: {
    version: 8,
    sources: {
      topo: {
        type: "raster",
        tiles: [
          "https://basemap.nationalmap.gov/arcgis/rest/services/USGSTopo/MapServer/tile/{z}/{y}/{x}",
        ],
        tileSize: 256,
        maxzoom: 16,
        attribution: "Basemap: USGS The National Map",
      },
    },
    layers: [{ id: "topo", type: "raster", source: "topo" }],
  },
});
map.addControl(new maplibregl.NavigationControl(), "top-right");
map.addControl(new maplibregl.ScaleControl({ unit: "metric" }), "bottom-right");

// Class colours for route lines (opaque versions of the tile palette).
let classColors: string[] = ["#ffffff", "#38a800", "#005ce6", "#141414", "#dc1e1e"];

function rgb([r, g, b]: number[]): string {
  return `rgb(${r}, ${g}, ${b})`;
}

function fmt(v: number | null | undefined, digits = 0, unit = ""): string {
  return v === null || v === undefined ? "–" : `${v.toFixed(digits)}${unit}`;
}

/** Escape text from forecast files before it goes into HTML. */
function esc(v: unknown): string {
  return String(v ?? "").replace(
    /[&<>"']/g,
    (c) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" })[c]!,
  );
}

/** The route-wide forecast block: source, zones, highest danger. */
function forecastSummaryHtml(f: ForecastSummary): string {
  const zones = f.zones
    .map(
      (z) =>
        `<div>${esc(z.title ?? z.area_id)}: issued ${esc(z.issued ?? "–")}, expires ${esc(z.expires ?? "–")}` +
        `${z.expired ? ' <strong class="warn">EXPIRED</strong>' : ""}</div>`,
    )
    .join("");
  return `<div class="forecast">
    <h3>Forecast context</h3>
    ${zones || "<div>The route is outside every loaded forecast zone.</div>"}
    ${f.highest_danger ? `<div>Highest danger along the route: <strong>${esc(f.highest_danger.name)}</strong></div>` : ""}
    ${f.no_zone_m > 0 ? `<div>Outside every zone: ${(f.no_zone_m / 1000).toFixed(2)} km</div>` : ""}
    ${f.treeline_m ? "" : "<div>No treeline set: each stretch shows every elevation band.</div>"}
    <p class="fine">${esc(f.notice)} Source: <a href="${esc(f.source.url)}" target="_blank" rel="noopener">${esc(f.source.name)}</a>.</p>
  </div>`;
}

/** One line of forecast context for a stretch. */
function stretchForecastHtml(c: StretchForecast): string {
  if (!c.danger) return `<div class="fine">${esc(c.note ?? "No forecast for this day.")}</div>`;
  const listed = (c.problems ?? []).filter((p) => p.listed_here === true).map((p) => esc(p.type));
  const unclear = (c.problems ?? []).filter((p) => p.listed_here === null).length;
  return (
    `<div class="fine">${c.bands.map((b) => b.toUpperCase()).join("/")} · ` +
    `danger ${esc(c.highest_danger?.name ?? "not rated")} · ` +
    `problems here: ${listed.length ? listed.join(", ") : "none listed"}` +
    `${unclear ? ` (${unclear} undetermined)` : ""}${c.expired ? ' · <strong class="warn">expired</strong>' : ""}</div>`
  );
}

// ---------------------------------------------------------------- regions

async function loadRegions(): Promise<void> {
  const res = await getRegions();
  $("disclaimer").textContent = res.disclaimer;
  const select = $<HTMLSelectElement>("region");
  select.innerHTML = "";
  for (const r of res.regions) {
    const opt = document.createElement("option");
    opt.value = r.name;
    opt.textContent = r.name.replace(/_/g, " ");
    select.append(opt);
  }
  select.onchange = () => showRegion(res.regions.find((r) => r.name === select.value)!);
  if (res.regions.length > 0) await showRegion(res.regions[0]);
  else $("status").textContent = "No region builds on the server.";
}

async function showRegion(region: RegionInfo): Promise<void> {
  const url = apiUrl(`/v1/files/${region.name}/ates.pmtiles`);
  const archive = new PMTiles(url);
  protocol.add(archive);
  const header = await archive.getHeader();
  const meta = (await archive.getMetadata()) as { legend?: LegendEntry[]; attribution?: string };

  if (map.getLayer("ates")) map.removeLayer("ates");
  if (map.getSource("ates")) map.removeSource("ates");
  map.addSource("ates", {
    type: "raster",
    url: `pmtiles://${url}`,
    tileSize: 256,
    attribution: meta.attribution,
  });
  map.addLayer({
    id: "ates",
    type: "raster",
    source: "ates",
    paint: {
      // Classes are categories: never blend neighbouring pixels.
      "raster-resampling": "nearest",
      "raster-opacity": Number($<HTMLInputElement>("opacity").value),
    },
  });
  // Keep the route and the route being drawn above the overlay.
  for (const id of ["route-casing", "route", "draft", "draft-points"]) {
    if (map.getLayer(id)) map.moveLayer(id);
  }

  if (meta.legend) {
    classColors = meta.legend.map((l) => rgb(l.rgba));
    renderLegend(meta.legend);
  }
  map.fitBounds(
    [
      [header.minLon, header.minLat],
      [header.maxLon, header.maxLat],
    ],
    { padding: 40, duration: 0 },
  );
  $("status").textContent = `${region.name.replace(/_/g, " ")}: ${region.rows} × ${region.cols} cells at ${region.cell_size_m} m. Click the map for details.`;
}

function renderLegend(legend: LegendEntry[]): void {
  const el = $("legend");
  el.innerHTML = "";
  for (const l of legend) {
    if (l.class === 0) continue;
    const row = document.createElement("div");
    row.className = "legend-row";
    const swatch = document.createElement("span");
    swatch.className = "swatch";
    swatch.style.background = `rgba(${l.rgba[0]}, ${l.rgba[1]}, ${l.rgba[2]}, ${Math.max(l.rgba[3] / 255, 0.6)})`;
    row.append(swatch, `${l.class} ${l.name}`);
    el.append(row);
  }
}

$<HTMLInputElement>("opacity").oninput = (e) => {
  if (map.getLayer("ates")) {
    map.setPaintProperty("ates", "raster-opacity", Number((e.target as HTMLInputElement).value));
  }
};

// ---------------------------------------------------------------- point details

function pointHtml(p: PointResponse): string {
  const yes = (b: boolean | null) => (b === null ? "–" : b ? "yes" : "no");
  return `
    <div class="popup">
      <strong>${p.ates_class === null ? "No class" : `Class ${p.ates_class}: ${p.ates_class_name}`}</strong>
      <table>
        <tr><td>Elevation</td><td>${fmt(p.elevation_m, 0, " m")}</td></tr>
        <tr><td>Slope</td><td>${fmt(p.slope_deg, 0, "°")}</td></tr>
        <tr><td>Aspect</td><td>${p.aspect ?? "flat"} ${p.aspect_deg === null ? "" : `(${p.aspect_deg.toFixed(0)}°)`}</td></tr>
        <tr><td>Canopy cover</td><td>${fmt(p.forest_canopy_pct, 0, " %")}</td></tr>
        <tr><td>Release area</td><td>${yes(p.in_release_area)}</td></tr>
        <tr><td>Avalanche path</td><td>${yes(p.on_avalanche_path)}</td></tr>
        <tr><td>Travel angle</td><td>${fmt(p.fp_travel_angle_deg, 0, "°")}</td></tr>
        <tr><td>Overhead exposure</td><td>${fmt(p.overhead)}</td></tr>
      </table>
      <p class="fine">Modeled terrain, not an avalanche forecast.</p>
    </div>`;
}

async function showPoint(e: MapMouseEvent): Promise<void> {
  const { lng, lat } = e.lngLat;
  const popup = new maplibregl.Popup({ maxWidth: "280px" })
    .setLngLat(e.lngLat)
    .setHTML("<div class='popup'>Loading…</div>")
    .addTo(map);
  try {
    popup.setHTML(pointHtml(await getPoint(lng, lat)));
  } catch (err) {
    popup.setHTML(`<div class="popup">${(err as Error).message}</div>`);
  }
}

// ---------------------------------------------------------------- routes

let drawing = false;
let draft: [number, number][] = [];

function setDraft(): void {
  (map.getSource("draft") as GeoJSONSource | undefined)?.setData({
    type: "FeatureCollection",
    features: [
      ...(draft.length > 1
        ? [{ type: "Feature" as const, properties: {}, geometry: { type: "LineString" as const, coordinates: draft } }]
        : []),
      ...draft.map((c) => ({ type: "Feature" as const, properties: {}, geometry: { type: "Point" as const, coordinates: c } })),
    ],
  });
}

function setDrawing(on: boolean): void {
  drawing = on;
  $("draw").textContent = on ? "Finish route" : "Draw route";
  map.getCanvas().style.cursor = on ? "crosshair" : "";
  $("draw-hint").hidden = !on;
}

async function runRoute(body: string, isGpx: boolean): Promise<void> {
  $("route-panel").hidden = false;
  $("route-summary").textContent = "Evaluating…";
  $("stretches").innerHTML = "";
  try {
    showReport(await evaluateRoute(body, isGpx));
  } catch (err) {
    $("route-summary").textContent = (err as Error).message;
  }
}

/** Padding that keeps fitted bounds clear of the side panel. */
function fitPadding(extra: number): maplibregl.PaddingOptions {
  const panel = $("panel").getBoundingClientRect();
  const beside = window.innerWidth > 600;
  return {
    top: extra,
    right: extra,
    bottom: beside ? extra : window.innerHeight - panel.top + extra,
    left: beside ? panel.right + extra : extra,
  };
}

function showReport(rep: RouteReport): void {
  // The evaluated route replaces the draft, so its class colours show.
  draft = [];
  setDraft();
  (map.getSource("route") as GeoJSONSource).setData(rep as unknown as Parameters<GeoJSONSource["setData"]>[0]);
  const s = rep.summary;
  const km = (m: number) => (m / 1000).toFixed(2);
  const bars = [1, 2, 3, 4]
    .filter((c) => (s.class_m[c] ?? 0) > 0)
    .map(
      (c) =>
        `<div class="bar-row"><span class="swatch" style="background:${classColors[c]}"></span>` +
        `${c} ${s.class_names[c]}: ${km(s.class_m[c])} km (${((100 * s.class_m[c]) / s.total_m).toFixed(0)} %)</div>`,
    )
    .join("");
  $("route-summary").innerHTML = `
    <p><strong>${km(s.total_m)} km</strong> evaluated in ${s.region.replace(/_/g, " ")}.</p>
    ${bars}
    ${s.outside_region_m > 0 ? `<div>Outside the region: ${km(s.outside_region_m)} km</div>` : ""}
    <p>In modeled release areas: ${s.release_area_m.toFixed(0)} m<br>
       On modeled avalanche paths: ${s.avalanche_path_m.toFixed(0)} m</p>
    <p class="fine">${s.disclaimer} Check the current avalanche forecast.</p>
    ${s.forecast ? forecastSummaryHtml(s.forecast) : ""}`;

  const list = $("stretches");
  const exposed = rep.features.filter((f) => (f.properties.ates_class ?? 0) >= 3);
  list.innerHTML = exposed.length ? "<h3>Class 3–4 stretches</h3>" : "";
  for (const f of exposed) {
    const p = f.properties;
    const item = document.createElement("button");
    item.className = "stretch";
    item.innerHTML =
      `<span class="swatch" style="background:${classColors[p.ates_class ?? 0]}"></span>` +
      `${(p.start_m / 1000).toFixed(2)}–${(p.end_m / 1000).toFixed(2)} km · ${p.ates_class_name} · ` +
      `${p.length_m.toFixed(0)} m · ${p.dominant_aspect ?? "flat"} · ` +
      `${fmt(p.elevation_min_m)}–${fmt(p.elevation_max_m, 0, " m")}` +
      (p.forecast_context ?? []).map(stretchForecastHtml).join("");
    item.onclick = () => {
      const b = new maplibregl.LngLatBounds();
      for (const c of f.geometry.coordinates) b.extend(c);
      map.fitBounds(b, { padding: fitPadding(80), maxZoom: 16 });
    };
    list.append(item);
  }
  const all = new maplibregl.LngLatBounds();
  for (const f of rep.features) for (const c of f.geometry.coordinates) all.extend(c);
  if (!all.isEmpty()) map.fitBounds(all, { padding: fitPadding(40), maxZoom: 15 });
}

$("draw").onclick = () => {
  if (!drawing) {
    draft = [];
    setDraft();
    setDrawing(true);
    return;
  }
  setDrawing(false);
  if (draft.length < 2) return;
  const line = { type: "LineString", coordinates: draft };
  void runRoute(JSON.stringify(line), false);
};

$("clear").onclick = () => {
  draft = [];
  setDraft();
  setDrawing(false);
  (map.getSource("route") as GeoJSONSource).setData({ type: "FeatureCollection", features: [] });
  $("route-panel").hidden = true;
};

$<HTMLInputElement>("upload").onchange = async (e) => {
  const file = (e.target as HTMLInputElement).files?.[0];
  if (!file) return;
  const text = await file.text();
  const isGpx = file.name.toLowerCase().endsWith(".gpx") || text.trimStart().startsWith("<");
  draft = [];
  setDraft();
  void runRoute(text, isGpx);
  (e.target as HTMLInputElement).value = "";
};

map.on("click", (e: MapMouseEvent) => {
  if (drawing) {
    draft.push([e.lngLat.lng, e.lngLat.lat]);
    setDraft();
  } else {
    void showPoint(e);
  }
});

map.on("load", async () => {
  map.addSource("route", { type: "geojson", data: { type: "FeatureCollection", features: [] } });
  map.addSource("draft", { type: "geojson", data: { type: "FeatureCollection", features: [] } });
  map.addLayer({
    id: "route-casing",
    type: "line",
    source: "route",
    paint: { "line-color": "#ffffff", "line-width": 7 },
    layout: { "line-cap": "round", "line-join": "round" },
  });
  map.addLayer({
    id: "route",
    type: "line",
    source: "route",
    paint: {
      "line-width": 4,
      "line-color": [
        "match",
        ["coalesce", ["get", "ates_class"], -1],
        1, "#38a800",
        2, "#005ce6",
        3, "#141414",
        4, "#dc1e1e",
        "#888888",
      ],
    },
    layout: { "line-cap": "round", "line-join": "round" },
  });
  map.addLayer({
    id: "draft",
    type: "line",
    source: "draft",
    filter: ["==", ["geometry-type"], "LineString"],
    paint: { "line-color": "#ff7f00", "line-width": 3, "line-dasharray": [2, 1] },
  });
  map.addLayer({
    id: "draft-points",
    type: "circle",
    source: "draft",
    filter: ["==", ["geometry-type"], "Point"],
    paint: { "circle-radius": 4, "circle-color": "#ff7f00", "circle-stroke-color": "#fff", "circle-stroke-width": 1 },
  });
  try {
    await loadRegions();
  } catch (err) {
    $("status").textContent = `Cannot reach the API: ${(err as Error).message}`;
  }
});
