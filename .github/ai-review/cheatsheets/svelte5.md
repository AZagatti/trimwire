# Svelte 5 current facts (injected for frontend personas)

The model's training may predate Svelte 5. These are the CURRENT rules — trust them over memory:

- **Reactivity is runes**, not `$:`. `let x = 0` is NOT reactive; use `let x = $state(0)`. A plain `let` mutated in a handler will NOT update the UI.
- **Props are `$props()`**, not `export let`. `export let foo` is legacy and silently non-reactive with Svelte-5 class/state values: `let { foo } = $props()`.
- **Event handlers are attributes**, not directives: `onclick={...}`, NOT `on:click={...}`. `on:click` is Svelte-4 syntax.
- **Event modifiers were REMOVED**: `on:click|preventDefault`, `|stopPropagation`, `|once`, `|self`, `|trusted` do not exist. Wrap the handler (helpers importable from `svelte/legacy`) or call `e.preventDefault()` inside.
- **`<script context="module">` → `<script module>`** (the `context="module"` attribute is removed).
- **`createEventDispatcher()` is deprecated** — use callback props instead.
- **`beforeUpdate`/`afterUpdate` removed** — use `$effect.pre` / `$effect`.
- **Derived is `$derived(...)`** and **effects are `$effect(...)`**. A `$derived` created inside an active `$effect` (or a constructor run from within one) is registered as a dependency of that effect — re-running the effect re-creates the object instead of updating in place. Watch for `$derived`/`$state` inside `$effect`/`untrack` bodies.
- **`$derived` class fields have no production setter** — assigning to one silently creates a shadow own-property (a throwing setter exists only in dev builds). Flag assignments to `$derived` fields.
- **Compiler AST**: `MustacheTag` was renamed to `ExpressionTag`; public types live under `AST.*` in `svelte/compiler` (don't import private/internal compiler types).
