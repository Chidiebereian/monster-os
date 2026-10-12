import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";

// The dashboard talks only to the MonsterOS api-gateway. In dev we proxy /api
// to it so the browser hits same-origin (no CORS). Override the target with
// VITE_GATEWAY if the gateway runs elsewhere.
const gateway = process.env.VITE_GATEWAY ?? "http://localhost:8090";

export default defineConfig({
  plugins: [react()],
  server: {
    port: 5190,
    proxy: {
      "/api": { target: gateway, changeOrigin: true },
    },
  },
});
