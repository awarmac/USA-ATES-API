import { defineConfig } from "vite";

// In development, /v1 is proxied to a local ates-api so the frontend and
// API share an origin (no CORS needed; PMTiles range requests just work).
export default defineConfig({
  server: {
    proxy: {
      "/v1": process.env.ATES_API ?? "http://127.0.0.1:8080",
    },
  },
  build: {
    outDir: "dist",
    chunkSizeWarningLimit: 1500,
  },
});
