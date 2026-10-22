import { defineConfig } from 'vite'
import react from '@vitejs/plugin-react'
import tailwindcss from '@tailwindcss/vite'

// Dev server on 5188 (5173 is taken by another project on the dev machine), proxying the API and the
// WebSocket to a gateway started with `make dev-backend` (dashboard on 127.0.0.1:8788, TLS off).
export default defineConfig({
  plugins: [react(), tailwindcss()],
  server: {
    port: 5188,
    strictPort: true,
    proxy: {
      '/api': { target: 'http://127.0.0.1:8788' },
      '/ws': { target: 'ws://127.0.0.1:8788', ws: true },
    },
  },
  build: {
    outDir: 'dist',
    emptyOutDir: true,
    sourcemap: false,
  },
})
