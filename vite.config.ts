import { defineConfig } from "vite";

export default defineConfig({
  clearScreen: false,
  server: {
    port: 1420,
    strictPort: true,
    watch: {
      // Never watch Rust build output: cargo locks .exe files while
      // compiling and Vite's watcher crashes with EBUSY on Windows.
      ignored: ["**/src-tauri/**", "**/target/**", "**/node_modules/**", "**/dist/**"],
    },
  },
  envPrefix: ["VITE_", "TAURI_"],
  build: {
    target: "chrome105",
    minify: "esbuild",
    sourcemap: false,
    rollupOptions: {
      input: {
        main: "src/main.html",
        overlay: "src/overlay.html",
        editor: "src/editor.html",
        settings: "src/settings.html",
        history: "src/history.html",
      },
    },
  },
});
