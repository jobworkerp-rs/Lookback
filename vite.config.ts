/// <reference types="vitest/config" />
import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";
import path from "node:path";
import { VITE_WATCH_IGNORED } from "./src/lib/viteWatchIgnored";

// Tauri dev server: fixed port so src-tauri/tauri.conf.json can point at it.
const HOST = process.env.TAURI_DEV_HOST;

export default defineConfig({
  plugins: [react()],
  resolve: {
    alias: {
      "@": path.resolve(__dirname, "./src"),
    },
  },
  // Tauri expects a fixed dev server port and a port that won't clash with sidecar gRPC ports (9000, 9010).
  clearScreen: false,
  server: {
    port: 5180,
    strictPort: true,
    host: HOST || false,
    hmr: HOST
      ? { protocol: "ws", host: HOST, port: 5181 }
      : undefined,
    watch: {
      // Avoid watching Rust crate and Cargo output created outside Vite's lifecycle.
      ignored: VITE_WATCH_IGNORED,
    },
  },
  envPrefix: ["VITE_", "TAURI_ENV_"],
  build: {
    target: process.env.TAURI_ENV_PLATFORM === "windows" ? "chrome105" : "safari13",
    minify: !process.env.TAURI_ENV_DEBUG ? "esbuild" : false,
    sourcemap: !!process.env.TAURI_ENV_DEBUG,
  },
  test: {
    environment: "jsdom",
    setupFiles: ["./vitest.setup.ts"],
  },
});
