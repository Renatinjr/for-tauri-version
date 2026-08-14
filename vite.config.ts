import { defineConfig } from "vite";

// Tauri drives this: `beforeDevCommand` starts the dev server on a fixed port and points
// the webview at it, so the port must not float. `clearScreen: false` keeps Rust's
// compiler output visible when both are running in the same terminal.
export default defineConfig({
  clearScreen: false,
  server: {
    port: 1420,
    strictPort: true,
    watch: { ignored: ["**/src-tauri/**"] },
  },
  build: {
    // WebView2 on a supported Windows build is well past this; the floor only exists so
    // Vite does not down-level to something ancient.
    target: "chrome105",
    sourcemap: true,
  },
});
