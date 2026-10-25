import { defineConfig } from 'vite';
import vue from '@vitejs/plugin-vue';

export default defineConfig({
  base: '/ui/',
  plugins: [vue({ template: { transformAssetUrls: false } })],
  publicDir: false,
  build: {
    // Fixed names let the Rust router embed a bounded set of offline assets.
    cssCodeSplit: false,
    rollupOptions: { output: { entryFileNames: 'app.js', assetFileNames: 'style.[ext]', inlineDynamicImports: true } },
  },
  test: { environment: 'jsdom', include: ['src/**/*.test.js'] },
});
