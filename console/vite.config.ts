import react from "@vitejs/plugin-react";
import { defineConfig } from "vite";

// In dev, the API stays on `mmx serve`. The Host header is left alone, so the server's origin check
// sees the same address the browser does.
const target = process.env.MMX_URL ?? "http://127.0.0.1:8093";

export default defineConfig({
  plugins: [react()],
  server: {
    port: 5190,
    strictPort: true,
    host: "127.0.0.1",
    proxy: { "/admin": { target, changeOrigin: false } },
  },
  // Nothing inlined, because the server's content security policy refuses `data:` fonts.
  build: { outDir: "dist", assetsDir: "assets", assetsInlineLimit: 0 },
});
