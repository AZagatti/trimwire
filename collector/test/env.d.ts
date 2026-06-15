// Test-only ambient types for the Workers-pool route tests. Kept as a SCRIPT
// file (no top-level import/export) so the `declare module` blocks below CREATE
// ambient modules rather than augmenting (which would require them to pre-exist).

// Brings in the `cloudflare:test` module types (env, createExecutionContext, …).
/// <reference types="@cloudflare/vitest-pool-workers/types" />

// schema.sql is imported as a raw string (vite `?raw`) to seed the test D1.
declare module "*.sql?raw" {
  const content: string;
  export default content;
}
