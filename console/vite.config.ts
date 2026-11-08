import react from "@vitejs/plugin-react";
import { defineConfig, loadEnv } from "vite";

// In dev, the API stays on `mmx serve`. The Host header is left alone, so the server's origin check
// sees the same address the browser does.
// `.env.local` can hold MMX_URL too, which repoints a running dev server without restarting it.
export default defineConfig(({ mode }) => {
  const target =
    process.env.MMX_URL ?? loadEnv(mode, process.cwd(), "MMX_").MMX_URL ?? "http://127.0.0.1:8093";
  return {
    plugins: [react()],
    server: {
      port: 5190,
      strictPort: true,
      host: "127.0.0.1",
      proxy: { "/admin": { target, changeOrigin: false } },
    },
    // Nothing inlined, because the server's content security policy refuses `data:` fonts.
    build: { outDir: "dist", assetsDir: "assets", assetsInlineLimit: 0 },
  };
});
