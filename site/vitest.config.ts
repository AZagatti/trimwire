import { defineConfig } from "vitest/config";

// happy-dom gives the renderer a DOM so mount()/render logic is unit-tested
// without a browser. Tests live next to the modules they cover (src/**/*.test.ts).
export default defineConfig({
  test: {
    environment: "happy-dom",
    include: ["src/**/*.test.ts"],
  },
});
