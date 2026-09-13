import { defineConfig } from "vite";

export default defineConfig({
  clearScreen: false,
  server: {
    port: 1420,
    strictPort: true,
    watch: {
      // Don't watch Rust build output: Windows locks .dll/.exe files while
      // rustc links them, which crashes Vite's file watcher with EBUSY.
      ignored: ["**/src-tauri/**"],
    },
  },
  build: {
    outDir: "dist",
    target: "es2021",
  },
});
