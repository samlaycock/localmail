import { resolve } from "node:path"
import tailwindcss from "@tailwindcss/vite"
import react from "@vitejs/plugin-react"
import { defineConfig, lazyPlugins } from "vite-plus"

// `bun run dev` serves the app with hot reload and sends the API to a running localmail
// (`cargo run`). Set LOCALMAIL_URL to use another one.
const server = process.env.LOCALMAIL_URL ?? "http://localhost:9402"

export default defineConfig({
  // `vp check` formats (Oxfmt), lints (Oxlint) and type-checks (tsrs) in one pass.
  lint: {
    // Oxlint's correctness rules, with the React plugin's among them: the hooks and React
    // Compiler rules. Stock shadcn components aren't linted, since they're never edited.
    plugins: ["oxc", "typescript", "unicorn", "react"],
    categories: { correctness: "error" },
    env: { browser: true },
    ignorePatterns: ["dist/**", "src/components/ui/**"],
    options: { typeAware: true, typeCheck: true },
    jsPlugins: [{ name: "vite-plus", specifier: "vite-plus/oxlint-plugin" }],
    rules: {
      "react/rules-of-hooks": "error",
      "vite-plus/prefer-vite-plus-imports": "error",
    },
  },
  fmt: {
    // shadcn's code style, so app code matches the stock components.
    semi: false,
    trailingComma: "es5",
    printWidth: 80,
    sortTailwindcss: { stylesheet: "src/index.css", functions: ["cn", "cva"] },
    // Stock shadcn output (the components, and the theme in index.css) stays exactly as the
    // CLI wrote it.
    ignorePatterns: ["dist/**", "src/components/ui/**", "src/index.css"],
  },
  plugins: lazyPlugins(() => [react(), tailwindcss()]),
  resolve: {
    alias: {
      "@": resolve(import.meta.dirname, "./src"),
    },
  },
  server: {
    proxy: { "/dev/api": server },
  },
  build: {
    // One bundle (about 225 kB gzipped) is fine for a page served from localhost.
    chunkSizeWarningLimit: 1000,
  },
})
