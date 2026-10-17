import { svelte } from "@sveltejs/vite-plugin-svelte";
import { defineConfig } from "vitest/config";

export default defineConfig({
  // Compile .svelte / .svelte.ts so components can be server-rendered in tests.
  // configFile:false keeps the SvelteKit config (and its tsconfig) out of it.
  plugins: [svelte({ configFile: false, compilerOptions: { runes: true } })],
  // Do not inherit the app tsconfig (it extends ./.svelte-kit/tsconfig.json,
  // which only exists after `svelte-kit sync`).
  esbuild: { tsconfigRaw: { compilerOptions: { target: "es2022", module: "esnext", strict: true } } },
  test: {
    include: ["src/**/*.test.ts"],
    environment: "node",
  },
});
