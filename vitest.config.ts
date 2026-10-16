import { defineConfig } from "vitest/config";

export default defineConfig({
  // Do not inherit the app tsconfig (it extends ./.svelte-kit/tsconfig.json,
  // which only exists after `svelte-kit sync`).
  esbuild: { tsconfigRaw: { compilerOptions: { target: "es2022", module: "esnext", strict: true } } },
  test: {
    include: ["src/**/*.test.ts"],
    environment: "node",
  },
});
