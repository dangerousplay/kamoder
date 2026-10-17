import path from "path"
import tailwindcss from "@tailwindcss/vite"
import react from "@vitejs/plugin-react"
import { defineConfig } from "vite"

// Camera (or itest forwarded port 80) that serves /api during `npm run dev`.
const target = process.env.ANYKA_URL ?? "http://127.0.0.1:8080"

// https://vite.dev/config/
export default defineConfig({
  base: "/",
  plugins: [react(), tailwindcss()],
  resolve: {
    alias: {
      "@": path.resolve(import.meta.dirname, "./src"),
    },
  },
  server: {
    proxy: {
      "/api": {
        target,
        changeOrigin: true,
        // Audio talk/listen are WebSockets.
        ws: true,
        // SSE log streams stay open indefinitely: no proxy timeouts.
        timeout: 0,
        proxyTimeout: 0,
        configure: (proxy) => {
          proxy.on("proxyRes", (proxyRes, _req, res) => {
            const type = proxyRes.headers["content-type"] ?? ""
            if (type.startsWith("text/event-stream")) {
              // Never buffer/transform event streams; flush headers now.
              proxyRes.headers["cache-control"] = "no-cache, no-transform"
              proxyRes.headers["x-accel-buffering"] = "no"
              res.flushHeaders?.()
            }
          })
        },
      },
    },
  },
  // Audio worklets (`?worker&url`) are loaded by audioWorklet.addModule(),
  // which takes ES modules.
  worker: { format: "es" },
  build: {
    outDir: "dist",
    emptyOutDir: true,
    sourcemap: false,
    target: "es2022",
    reportCompressedSize: true,
    // react-dom + react-router dominate the entry chunk (~160 KiB gz).
    chunkSizeWarningLimit: 600,
  },
})
