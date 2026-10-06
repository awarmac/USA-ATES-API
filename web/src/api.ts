// Typed client for the ates-api HTTP API. These types mirror
// crates/ates-api/src/types.rs; keep them in sync.

export interface Provenance {
  tool_version: string;
  model: string;
  region: string | null;
  config_fnv1a64: string | null;
}

export interface RegionInfo {
  name: string;
  bbox_wgs84: [number, number, number, number] | null;
  epsg: number;
  cell_size_m: number;
  rows: number;
  cols: number;
  cells_per_class: [number, number, number, number, number];
  files: string[];
  manifest: Record<string, unknown>;
}

export interface RegionsResponse {
  regions: RegionInfo[];
  disclaimer: string;
}

export interface PointResponse {
  lon: number;
  lat: number;
  epsg: number;
  x: number;
  y: number;
  ates_class: number | null;
  ates_class_name: string | null;
  elevation_m: number | null;
  slope_deg: number | null;
  aspect_deg: number | null;
  aspect: string | null;
  forest_canopy_pct: number | null;
  in_release_area: boolean | null;
  on_avalanche_path: boolean | null;
  fp_travel_angle_deg: number | null;
  overhead: number | null;
  disclaimer: string;
  provenance: Provenance;
}

export interface StretchProperties {
  ates_class: number | null;
  ates_class_name: string | null;
  outside_region: boolean;
  part: number;
  start_m: number;
  end_m: number;
  length_m: number;
  elevation_min_m: number | null;
  elevation_max_m: number | null;
  dominant_aspect: string | null;
  aspect_m: Record<string, number>;
  release_area_m: number;
  avalanche_path_m: number;
  max_overhead: number | null;
  /** Present when the server has forecasts loaded; one entry per zone. */
  forecast_context?: StretchForecast[];
}

/** A rating on the danger scale; `level` is null when not rated. */
export interface DangerValue {
  level: number | null;
  name: string;
}

export interface ForecastZone {
  id: string;
  area_id: string;
  title: string | null;
  issued: string | null;
  expires: string | null;
  /** null when the expiry time is unknown. */
  expired: boolean | null;
}

export interface StretchForecast extends ForecastZone {
  bands: ("btl" | "tln" | "alp")[];
  bands_from: string;
  day: string | null;
  note?: string;
  danger?: Record<string, DangerValue>;
  highest_danger?: DangerValue | null;
  problems?: {
    type: string;
    likelihood: string | null;
    size_min: string | null;
    size_max: string | null;
    /** true listed, false not listed, null cannot be told. */
    listed_here: boolean | null;
    matched_locations: string[];
    undecoded_locations: string[];
  }[];
}

export interface ForecastSummary {
  source: { name: string; url: string; retrieved: string | null };
  notice: string;
  day: number;
  treeline_m: [number, number] | null;
  zones: ForecastZone[];
  highest_danger: DangerValue | null;
  no_zone_m: number;
}

export interface RouteSummary {
  total_m: number;
  class_m: Record<string, number>;
  class_names: string[];
  nodata_m: number;
  outside_region_m: number;
  release_area_m: number;
  avalanche_path_m: number;
  max_class: number | null;
  disclaimer: string;
  region: string;
  provenance: Provenance;
  /** Present when the server has forecasts loaded. */
  forecast?: ForecastSummary;
}

export interface RouteReport {
  type: "FeatureCollection";
  summary: RouteSummary;
  features: {
    type: "Feature";
    geometry: { type: "LineString"; coordinates: [number, number][] };
    properties: StretchProperties;
  }[];
}

/** API base: same origin by default (the Vite dev server proxies /v1). */
const BASE: string = import.meta.env.VITE_API_URL ?? "";

async function json<T>(res: Response): Promise<T> {
  const body = await res.json();
  if (!res.ok) {
    throw new Error(body?.error ?? `HTTP ${res.status}`);
  }
  return body as T;
}

export function apiUrl(path: string): string {
  return new URL(BASE + path, window.location.href).toString();
}

export async function getRegions(): Promise<RegionsResponse> {
  return json(await fetch(apiUrl("/v1/regions")));
}

export async function getPoint(lon: number, lat: number): Promise<PointResponse> {
  const q = new URLSearchParams({ lon: String(lon), lat: String(lat) });
  return json(await fetch(apiUrl(`/v1/point?${q}`)));
}

/** Evaluate a route given as GeoJSON text or GPX text. */
export async function evaluateRoute(body: string, isGpx: boolean): Promise<RouteReport> {
  const res = await fetch(apiUrl("/v1/route/evaluate"), {
    method: "POST",
    headers: { "Content-Type": isGpx ? "application/gpx+xml" : "application/geo+json" },
    body,
  });
  return json(res);
}
