import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";

// Tauri loads the built files from disk; the dev server is also used for browser-based testing.
export default defineConfig({
  plugins: [react()],
  base: "./",
  clearScreen: false,
  server: { port: 5173, strictPort: true, host: "127.0.0.1" },
  build: {
    target: "es2022",
    sourcemap: false,
    chunkSizeWarningLimit: 1500,
  },
});
