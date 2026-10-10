import { defineConfig } from "vite";

// Tauri serves the dev build to a phone over the LAN, so the dev server has to
// listen beyond loopback when TAURI_DEV_HOST says where.
const host = process.env.TAURI_DEV_HOST;

export default defineConfig({
  clearScreen: false,
  server: {
    port: 1420,
    strictPort: true,
    host: host || false,
    hmr: host ? { protocol: "ws", host, port: 1421 } : undefined,
    watch: { ignored: ["**/src-tauri/**"] },
  },
  build: {
    target: "es2022",
  },
});
