import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";

// In dev the Vite server serves the UI and proxies the two server routes, so
// `npm run dev` gives hot reload against a `cargo run` server. In production
// the Rust server serves `dist/` itself and no proxy is involved.
export default defineConfig({
  plugins: [react()],
  server: {
    port: 5173,
    proxy: {
      "/api": "http://127.0.0.1:4321",
      "/ws": { target: "ws://127.0.0.1:4321", ws: true },
    },
  },
  build: {
    outDir: "dist",
    sourcemap: true,
  },
});
