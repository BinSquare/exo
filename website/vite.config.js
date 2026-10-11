import { fileURLToPath } from "node:url";

import tailwindcss from "@tailwindcss/vite";
import react from "@vitejs/plugin-react";
import { defineConfig } from "vite";

export default defineConfig({
  root: fileURLToPath(new URL(".", import.meta.url)),
  resolve: {
    alias: {
      "@": fileURLToPath(new URL("src", import.meta.url)),
    },
  },
  plugins: [
    react(),
    tailwindcss(),
    {
      name: "exo-html-dev-routes",
      configureServer(server) {
        server.middlewares.use((request, _response, next) => {
          const page = request.url?.match(/^\/(chat|v2)\/?(\?.*)?$/);
          if (page) {
            request.url = `/${page[1]}.html${page[2] ?? ""}`;
          }
          next();
        });
      },
    },
  ],
  build: {
    outDir: "dist",
    emptyOutDir: true,
    rollupOptions: {
      input: {
        chat: fileURLToPath(new URL("chat.html", import.meta.url)),
        main: fileURLToPath(new URL("index.html", import.meta.url)),
        v2: fileURLToPath(new URL("v2.html", import.meta.url)),
      },
    },
  },
});
