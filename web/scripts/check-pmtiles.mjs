// Validate a PMTiles archive written by `ates build-tiles` with the
// reference `pmtiles` library: header, metadata, and every tile in the
// declared bounds and zoom range must decode as a WebP image.
//
// usage: node scripts/check-pmtiles.mjs ../data/regions/cameron_pass/ates.pmtiles

import { open } from "node:fs/promises";
import { PMTiles } from "pmtiles";

const path = process.argv[2];
if (!path) {
  console.error("usage: node scripts/check-pmtiles.mjs FILE.pmtiles");
  process.exit(2);
}
const fh = await open(path, "r");

/** A pmtiles Source reading byte ranges from a local file. */
const source = {
  getKey: () => path,
  async getBytes(offset, length) {
    const buf = Buffer.alloc(length);
    const { bytesRead } = await fh.read(buf, 0, length, offset);
    return { data: buf.buffer.slice(buf.byteOffset, buf.byteOffset + bytesRead) };
  },
};

const p = new PMTiles(source);
const h = await p.getHeader();
const meta = await p.getMetadata();
console.log(
  `header: spec v${h.specVersion}, tile type ${h.tileType}, zoom ${h.minZoom}-${h.maxZoom}, ` +
    `${h.numAddressedTiles} addressed / ${h.numTileEntries} entries / ${h.numTileContents} contents`,
);
console.log(`bounds: ${[h.minLon, h.minLat, h.maxLon, h.maxLat].map((v) => v.toFixed(4)).join(", ")}`);
console.log(`metadata: ${meta.name}; legend ${meta.legend.map((l) => l.name).join(" / ")}`);

const lon2x = (lon, z) => ((lon + 180) / 360) * 2 ** z;
const lat2y = (lat, z) => {
  const r = (lat * Math.PI) / 180;
  return ((1 - Math.asinh(Math.tan(r)) / Math.PI) / 2) * 2 ** z;
};

let found = 0;
let bad = 0;
for (let z = h.minZoom; z <= h.maxZoom; z++) {
  const [x0, x1] = [Math.floor(lon2x(h.minLon, z)), Math.floor(lon2x(h.maxLon, z))];
  const [y0, y1] = [Math.floor(lat2y(h.maxLat, z)), Math.floor(lat2y(h.minLat, z))];
  for (let x = x0; x <= x1; x++) {
    for (let y = y0; y <= y1; y++) {
      const t = await p.getZxy(z, x, y);
      if (!t) continue;
      found++;
      const b = new Uint8Array(t.data);
      const riff = String.fromCharCode(...b.slice(0, 4));
      const webp = String.fromCharCode(...b.slice(8, 12));
      if (riff !== "RIFF" || webp !== "WEBP") bad++;
    }
  }
}
await fh.close();
console.log(`tiles read: ${found}, not WebP: ${bad}`);
if (found !== h.numAddressedTiles || bad > 0 || h.tileType !== 4) {
  console.error("FAIL");
  process.exit(1);
}
console.log("PASS");
