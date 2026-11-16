import { defineConfig } from "tsup";

export default defineConfig([
  {
    entry: ["src/index.ts"],
    format: ["esm"],
    target: "node22",
    outDir: "dist",
    clean: true,
    sourcemap: true,
    dts: true,
  },
  {
    entry: ["src/sdk/index.ts"],
    format: ["esm"],
    target: "node22",
    outDir: "dist/sdk",
    sourcemap: true,
    dts: true,
  },
  {
    entry: ["src/mcp-server.ts"],
    format: ["esm"],
    target: "node22",
    outDir: "dist",
    sourcemap: true,
    dts: false,
  },
]);
