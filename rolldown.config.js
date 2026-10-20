import { defineConfig } from "rolldown";
import manifest from "./plugin.json" with { type: "json" };

// Decky provides these as window globals at runtime; @decky/api reads the plugin manifest at import time.
const deckyGlobals = {
  react: "SP_REACT",
  "react/jsx-runtime": "SP_JSX",
  "react-dom": "SP_REACTDOM",
  "@decky/ui": "DFL",
  "@decky/manifest": JSON.stringify(manifest),
};
const globalPrefix = "\0decky-global:";

// Same output contract as @decky/rollup: one ES module with a default plugin export.
export default defineConfig({
  input: "./src/index.tsx",
  platform: "browser",
  context: "window",
  transform: {
    define: { "process.env.NODE_ENV": JSON.stringify("production") },
  },
  plugins: [
    {
      // Resolved after the JSX transform, so the injected react/jsx-runtime import is covered too.
      name: "decky-globals",
      resolveId: (id) => (Object.hasOwn(deckyGlobals, id) ? globalPrefix + id : null),
      load: (id) =>
        id.startsWith(globalPrefix)
          ? `module.exports = ${deckyGlobals[id.slice(globalPrefix.length)]};`
          : null,
    },
  ],
  output: {
    dir: "dist",
    format: "esm",
    sourcemap: true,
    cleanDir: true,
  },
});
