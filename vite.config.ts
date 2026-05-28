import { defineConfig } from 'vite'
import { svelte } from '@sveltejs/vite-plugin-svelte'
import tailwindcss from '@tailwindcss/vite'
import Icons from 'unplugin-icons/vite'

// https://vite.dev/config/
export default defineConfig({
  plugins: [
    svelte(),
    tailwindcss(),
    // Tree-shaken icon components from any Iconify set via `~icons/<set>/<name>`.
    Icons({ compiler: 'svelte', autoInstall: false }),
  ],
  server: {
    port: 5173,
    strictPort: true,
  },
})
