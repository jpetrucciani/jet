import { defineConfig } from 'vite';
import solid from 'vite-plugin-solid';

export default defineConfig({
  plugins: [solid()],
  build: { target: 'es2022', outDir: 'dist', emptyOutDir: true },
  server: {
    // `pnpm vite` against a running jetmux-web on the default port.
    proxy: { '/api': { target: 'http://127.0.0.1:7681', ws: true } },
  },
});
