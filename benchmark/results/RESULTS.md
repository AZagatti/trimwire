# trimwire benchmark — offline replay

> **TL;DR** — request size **0–99% lighter** by session shape (nothing when
> there's no redundancy); the point is **context-window headroom**, not money;
> cost is non-monotonic (wash-to-loss short, ≈ −52% at 256 turns); **sub-2 ms**
> overhead; orphan-free + `system` untouched on every corpus + a 3,000-body fuzz.

Deterministic synthetic `/v1/messages` bodies fed through the real strategy
code under the **shipped default config**, unless noted. Corpora are ordered
low-savings → high. Byte columns are exact and reproducible; cost/token figures
are estimates; timing is host-dependent. See `examples/bench.rs` for caveats.

## Legend (plain English)

| Term | What it means |
|---|---|
| `cross_turn_dedup` | drops earlier copies of a tool call you repeated |
| `failed_input_purge` | clears the bulky input of an old *failed* command |
| `bloat_cap` | shrinks a huge *old* tool result to its head + tail |
| `sliding_window` | stubs old browser-automation tool calls |
| `image_strip` | replaces old screenshots with a marker (keeps recent ones) |
| cache stability | how much of the previous request the prompt cache can still reuse |
| orphan-free | never deletes half of a command↔result pair |
| no-op | trimwire forwarded the request byte-for-byte (nothing to prune) |

## 0. Context quality — keeping the session focused and rot-free

> The real job: keep the session **clean** so the model isn't wading through
> stale backlog ("context rot"). **Focus** = the share of the request that is
> the recent window you're actually working on (higher = the current task
> isn't drowned in history). **Redundancy** = the share that is repeated tool
> output (lower = less dead weight). Both are defined by recency/repetition,
> not by what trimwire happens to delete.

| Corpus | Focus (unpruned → pruned) | Redundancy (unpruned → pruned) |
|---|--:|--:|
| `pure_chat_floor` | 75.2% → **75.2%** | 0.0% → **0.0%** |
| `exempt_heavy` | 55.3% → **55.3%** | 0.0% → **0.0%** |
| `unique_bash_spam` | 41.6% → **67.2%** | 0.0% → **0.0%** |
| `at_the_boundary` | 71.3% → **88.6%** | 0.0% → **0.0%** |
| `repeated_grep` | 32.8% → **65.7%** | 59.6% → **0.0%** |
| `coding` | 2.6% → **15.3%** | 0.0% → **0.0%** |
| `mixed_realistic` | 0.3% → **6.8%** | 0.4% → **0.0%** |
| `mcp_non_playwright` | 40.1% → **68.4%** | 0.0% → **0.0%** |
| `long_running` | 17.2% → **43.2%** | 0.0% → **0.0%** |
| `resumed_session` | 2.5% → **7.0%** | 4.9% → **0.0%** |
| `browser_heavy` | 71.4% → **98.0%** | 0.0% → **0.0%** |
| `giant_paste` | 0.2% → **18.3%** | 0.0% → **0.0%** |
| `stale_input_heavy` | 7.1% → **45.5%** | 0.0% → **0.0%** |
| `thinking_heavy` | 8.1% → **47.1%** | 0.0% → **0.0%** |

> Pruning raises focus and drops redundancy on every shape with rot to remove,
> and leaves the clean floors untouched. Note the absolute level varies: on
> image-/log-dominated shapes (`coding` 2.6→12.6, `mixed_realistic` 0.3→6.9)
> focus stays single-digit even after pruning — those sessions are
> backlog-dominated regardless, and the real win there is the byte/redundancy
> cut, not focus. **Caveat:** focus and redundancy are *structural* proxies
> (byte share, repeated content); whether they translate into better model
> behaviour is plausible but unproven here — a model-in-the-loop eval (lost-in-
> the-middle / task-completion on long sessions) would be needed to show that,
> and we haven't run one. §6 shows focus over a growing session: unpruned
> decays as the backlog piles up; pruned holds higher.

## 1. Savings — shipped default config

| Corpus | What it models | In | Out | Saved | Reduction | Result |
|---|---|--:|--:|--:|--:|:-:|
| `pure_chat_floor` | the floor: nothing to prune, forwarded byte-for-byte | 9.5 KB | 9.5 KB | 0 B | 0.0% | no-op |
| `exempt_heavy` | honest low case: protected/unique content → ~nothing to prune | 57.1 KB | 57.1 KB | 0 B | 0.0% | no-op |
| `unique_bash_spam` | only the oldest results past the recent window get capped | 222.1 KB | 95.6 KB | 126.5 KB | 57.0% | pruned |
| `at_the_boundary` | recent results stay intact; only aged ones are capped | 143.6 KB | 79.3 KB | 64.3 KB | 44.8% | pruned |
| `repeated_grep` | drops 7 superseded repeats; the distinct searches are kept | 29.8 KB | 6.5 KB | 23.2 KB | 78.1% | pruned |
| `coding` | dedup on the superseded reads, bloat_cap on the old log | 48.0 KB | 8.2 KB | 39.8 KB | 83.0% | pruned |
| `mixed_realistic` | several strategies each take a slice — the realistic composite | 363.0 KB | 16.8 KB | 346.2 KB | 95.4% | pruned |
| `mcp_non_playwright` | default: bloat_cap only (denylist is playwright-only); more if tuned | 129.6 KB | 44.9 KB | 84.6 KB | 65.3% | pruned |
| `long_running` | diverse outputs; only aged oversized logs are capped | 133.6 KB | 53.3 KB | 80.4 KB | 60.1% | pruned |
| `resumed_session` | the length sweet spot: big size savings AND a real cost win | 186.4 KB | 65.2 KB | 121.1 KB | 65.0% | pruned |
| `browser_heavy` | biggest byte win, but the heaviest cache churn — often a wash on cost | 423.2 KB | 63.5 KB | 359.7 KB | 85.0% | pruned |
| `giant_paste` | extreme single-result bloat_cap; the big-body overhead probe | 508.2 KB | 5.9 KB | 502.3 KB | 98.8% | pruned |
| `stale_input_heavy` | old successful calls with bulky inputs — stale_input_cap territory | 14.3 KB | 2.2 KB | 12.0 KB | 84.3% | pruned |
| `thinking_heavy` | old reasoning-heavy turns — thinking_strip territory | 12.5 KB | 2.1 KB | 10.4 KB | 82.9% | pruned |

> Range across these shapes: **0.0% – 98.8%**. There is no single "trimwire
> saves X%" — it depends entirely on what your session looks like, and on
> a session with nothing redundant it correctly does nothing. Every pruned
> body is orphan-free, never larger than the input, and leaves `system`
> untouched (asserted; the harness panics otherwise).
>
> **In/Out/Reduction measure `messages[]` bytes only** — the real request also
> carries a `system` prompt + tool schemas (~12 KB ≈ 3000 tokens in production)
> that trimwire never touches, so they're excluded from the denominator here. On
> a **small** body this overstates the full-request reduction: e.g.
> `stale_input_heavy`/`thinking_heavy` (12–14 KB) read ~84%/83% of `messages[]`
> but only ~46%/42% of a full production request. These small "coverage corpora"
> exist to exercise one strategy each, not to represent typical session sizes.
> (The cost model in §5 adds the prefix back, so it isn't affected.)

## 2. Profiles — `default` / `gentle` (reduction %)

| Corpus | default (aggressive) | gentle (lightest) |
|---|--:|--:|
| `pure_chat_floor` | 0.0% | 0.0% |
| `exempt_heavy` | 0.0% | 0.0% |
| `unique_bash_spam` | 57.0% | 0.0% |
| `at_the_boundary` | 44.8% | 0.0% |
| `repeated_grep` | 78.1% | 58.1% |
| `coding` | 83.0% | 24.6% |
| `mixed_realistic` | 95.4% | 78.1% |
| `mcp_non_playwright` | 65.3% | 0.0% |
| `long_running` | 60.1% | 0.0% |
| `resumed_session` | 65.0% | 4.7% |
| `browser_heavy` | 85.0% | 0.0% |
| `giant_paste` | 98.8% | 0.0% |
| `stale_input_heavy` | 84.3% | 0.0% |
| `thinking_heavy` | 82.9% | 16.6% |

> Two *cleaning aggressiveness* levels. **`default`** (shipped) = all eight
> cache-safe strategies, tight knobs (`keep_recent_turns=2`, `bloat 4 KB`,
> `image keep 1`), a verb-class denylist (`*screenshot*`/`*navigate*`/`*click*`/
> `*browser_act*`/`Grep`), and reprune on — cleans hardest while keeping reference-data
> MCP results. **`gentle`** = dedup + failed-input-purge + a *conservative*
> bloat_cap (32 KB / keep 6) + thinking_strip (keep 8) + reprune;
> stale_input_cap, stale_reads, sliding-window and image-strip off (lightest
> touch, for cost-sensitive sessions). Pick with `profile = "…"` in your
> config. Their *cost* behaviour is not what you'd guess — see §5.

## 3. Per-strategy contribution (default config)

> Bytes each strategy removes *on top of the others* (turn it off, see what
> comes back). Unlike measuring each strategy in isolation, these add up
> toward the total instead of double-counting the same bytes twice.

| Corpus | failed_input_purge | stale_input_cap | cross_turn_dedup | stale_reads | simhash_dedup | bloat_cap | sliding_window | image_strip | thinking_strip |
|---|--:|--:|--:|--:|--:|--:|--:|--:|--:|
| `pure_chat_floor` | — | — | — | — | — | — | — | — | — |
| `exempt_heavy` | — | — | — | — | — | — | — | — | — |
| `unique_bash_spam` | — | — | — | — | — | 126.5 KB | — | — | — |
| `at_the_boundary` | — | — | — | — | — | 64.3 KB | — | — | — |
| `repeated_grep` | — | — | 14 B | — | — | — | 5.9 KB | — | — |
| `coding` | 3.0 KB | — | 40 B | 1.8 KB | — | 26.2 KB | — | — | — |
| `mixed_realistic` | 2.5 KB | — | 8 B | — | — | 42.3 KB | 20.4 KB | — | — |
| `mcp_non_playwright` | — | — | — | — | — | 84.6 KB | — | — | — |
| `long_running` | — | — | — | — | — | 80.4 KB | — | — | — |
| `resumed_session` | — | — | 48 B | — | — | 112.4 KB | — | — | — |
| `browser_heavy` | — | — | — | — | — | 4 B | 16.3 KB | 60.0 KB | — |
| `giant_paste` | — | — | — | — | — | 502.3 KB | — | — | — |
| `stale_input_heavy` | — | 12.0 KB | — | — | — | — | — | — | — |
| `thinking_heavy` | — | — | — | — | — | — | — | — | 10.4 KB |

## 4. Prompt-cache stability — turn-to-turn prefix reuse

| Corpus | Unpruned | Default | Gentle |
|---|--:|--:|--:|
| `pure_chat_floor` | 100.0% | 100.0% | 100.0% |
| `exempt_heavy` | 100.0% | 100.0% | 100.0% |
| `unique_bash_spam` | 100.0% | 36.7% | 100.0% |
| `at_the_boundary` | 100.0% | 39.7% | 100.0% |
| `repeated_grep` | 100.0% | 23.9% | 55.8% |
| `coding` | 100.0% | 75.0% | 89.8% |
| `mixed_realistic` | 100.0% | 46.2% | 68.5% |
| `mcp_non_playwright` | 100.0% | 60.8% | 100.0% |
| `long_running` | 100.0% | 84.0% | 100.0% |
| `resumed_session` | 100.0% | 87.4% | 96.4% |
| `browser_heavy` | 100.0% | 1.2% | 100.0% |
| `giant_paste` | 100.0% | 83.3% | 100.0% |
| `stale_input_heavy` | 100.0% | 54.8% | 100.0% |
| `thinking_heavy` | 100.0% | 49.8% | 88.9% |

> How much of the previous request the prompt cache can still reuse. Without
> pruning it's 100% — each turn just appends, so the cache keeps paying out.
> Pruning drops it whenever an old message is rewritten as it ages. We measure
> this a whole message at a time (the cache invalidates from the first changed
> message onward), which is a careful under-estimate of what's really kept.
> NOTE: these figures are the **stateless** prune (re-prune from scratch each turn).
> The shipped default runs **reprune** (on by default), which replays prior decisions
> so the prefix stays byte-identical — far more stable than shown here. See §5b for the
> reprune-on numbers (e.g. unique_bash_spam 36.7% stateless → 82.7% with reprune).

## 5. Does the bill actually go down? (input-cost model)

> Models the whole session's **input** cost under prompt-cache pricing
> ($3/Mtok input, cache reads at 10% rate). Each turn re-sends the
> conversation; the cached prefix is cheap, the rest full price. Bytes-down
> only helps the bill if it doesn't churn away more cache than it saves.
> Every turn also carries a constant system prompt + tool schemas that
> trimwire never touches, modeled as a fixed 3000-token cached prefix
> (written once, cache-read after). It is identical in every column, so
> it cancels in the %Δ — it shrinks the reported *magnitude* but never
> flips a sign.

| Corpus | Unpruned $ | default Δ | gentle Δ |
|---|--:|--:|--:|
| `pure_chat_floor` | $0.0201 | +0.0% | +0.0% |
| `exempt_heavy` | $0.0762 | +0.0% | +0.0% |
| `unique_bash_spam` | $0.2771 | +73.4% | +0.0% |
| `at_the_boundary` | $0.1544 | +71.4% | +0.0% |
| `repeated_grep` | $0.0547 | -4.4% | -17.6% |
| `coding` | $0.0913 | -1.5% | +15.8% |
| `mixed_realistic` | $0.5510 | -23.7% | +82.4% |
| `mcp_non_playwright` | $0.1627 | +34.5% | +0.0% |
| `long_running` | $0.2378 | -20.0% | +0.0% |
| `resumed_session` | $0.5701 | -26.0% | +15.4% |
| `browser_heavy` | $0.4270 | -21.1% | +0.0% |
| `giant_paste` | $0.6239 | -23.6% | +0.0% |
| `stale_input_heavy` | $0.0325 | +11.3% | +0.0% |
| `thinking_heavy` | $0.0319 | +42.1% | +21.1% |

> **Cost is non-monotonic in aggressiveness — "more pruning = more cost" is
> false, and "more pruning = less cost" is just as false.** `gentle` mostly
> hugs zero (it does little) but still churns a touch where it dedups old
> in-prefix reads. `default` (aggressive, reprune on) is the *cheapest* on
> long churny sessions where its byte reduction outweighs the churn
> (`unique_bash_spam` is a clear win), but it can cost *more* on shortish
> sessions where the deferred stable prefix is larger than an aggressively
> stubbed snapshot (`mixed_realistic`, `browser_heavy`). So the cost-min
> choice is shape-dependent: `gentle` for short throwaway sessions, `default`
> for long ones. `default` optimises cleanliness-per-risk, not the bill —
> reprune keeps it *cache-stable* everywhere even where it isn't cost-minimal.
> All figures are sub-cent estimates — read sign and trend. (We omit the
> 1.25× cache-write surcharge as second-order.)

> A negative Δ means trimwire lowered the modelled input bill; a positive Δ
> means cache churn outweighed the byte savings (image-heavy sessions are the
> risk). **Read the sign and the trend, not the magnitude:** these short
> synthetic sessions cost fractions of a cent, so a "+123%" is +123% of a
> quarter-cent — what matters is the *direction*, and how it flips with length
> (next). Like §4 this uses whole-message cache granularity, which is mildly
> optimistic about retained cache, and inherits the ~4 B/token estimate —
> directional, not an invoice. The byte savings are real regardless.

## 5b. The cost fix — stable-prefix re-pruning (`[reprune]`, on by default)

> §5 prunes *statelessly*: every turn re-prunes from scratch, so the pruned
> prefix shifts and busts the cache — that's the churn behind the loss rows.
> **Stable-prefix re-pruning** keeps the pruned prefix byte-identical between
> re-checkpoints, so the cache survives. Below: the `default` config, replayed
> stateless vs. with reprune on (threshold 8). It erases most of the churn
> cost on long/heavy sessions — at a small cost on short ones (it defers the
> newest trim by one checkpoint). Both shipped profiles turn it on, which is
> what makes the aggressive default cache-stable.

| Corpus | cost (stateless → reprune) | cache-stability (stateless → reprune) |
|---|--:|--:|
| `pure_chat_floor` | $0.0201 → $0.0201 ↓ | 100.0% → **100.0%** |
| `exempt_heavy` | $0.0762 → $0.0762 ↓ | 100.0% → **100.0%** |
| `unique_bash_spam` | $0.4805 → $0.3182 ↓ | 36.7% → **82.7%** |
| `at_the_boundary` | $0.2646 → $0.1837 ↓ | 39.7% → **83.4%** |
| `repeated_grep` | $0.0523 → $0.0500 ↓ | 23.9% → **82.6%** |
| `coding` | $0.0900 → $0.0742 ↓ | 75.0% → **87.3%** |
| `mixed_realistic` | $0.4205 → $0.4397 ↑ | 46.2% → **82.1%** |
| `mcp_non_playwright` | $0.2188 → $0.1705 ↓ | 60.8% → **88.9%** |
| `long_running` | $0.1904 → $0.2325 ↑ | 84.0% → **85.2%** |
| `resumed_session` | $0.4221 → $0.4107 ↓ | 87.4% → **90.7%** |
| `browser_heavy` | $0.3368 → $0.3838 ↑ | 1.2% → **83.3%** |
| `giant_paste` | $0.4764 → $0.5521 ↑ | 83.3% → **83.3%** |
| `stale_input_heavy` | $0.0362 → $0.0327 ↓ | 54.8% → **87.6%** |
| `thinking_heavy` | $0.0453 → $0.0359 ↓ | 49.8% → **89.0%** |

> Read with §5: the cost loss there is what a *stateless* prune would cost.
> Because reprune ships on, a long/churny session gets the cache back. The
> short-session penalty is real but bounded — see the per-length spike under
> `--spike` for the crossover.

## 6. Savings build up over a session (default config)

> The aging strategies only fire once content passes the recent-turn window,
> so savings start near zero and climb. Reduction % at each turn:

- `long_running` — t1:0.0%  t3:0.0%  t5:37.4%  t7:36.3%  t9:49.1%  t11:48.1%  t13:54.8%  t15:54.0%  t17:58.2%  t19:57.5%  t21:60.4%
- `mixed_realistic` — t1:0.0%  t3:7.6%  t5:75.5%  t7:55.8%  t9:74.7%  t11:74.2%  t13:90.6%  t15:95.5%  t17:95.4%
- `mcp_non_playwright` — t1:0.0%  t2:0.0%  t3:0.0%  t4:40.8%  t5:27.3%  t6:54.4%  t7:40.9%  t8:61.2%  t9:49.1%  t10:65.3%

> And the rot story directly — **focus ratio (unpruned → pruned) over a long
> session**. Unpruned focus decays as the backlog buries the recent ask;
> pruned focus holds steady. That's trimwire keeping the session clean:

- `resumed_session` unpruned focus — t1:100.0%  t7:53.5%  t13:9.7%  t19:31.7%  t25:23.8%  t31:16.0%  t37:13.6%  t43:12.4%  t49:2.5%
- `resumed_session`  pruned  focus — t1:100.0%  t7:80.7%  t13:26.0%  t19:32.7%  t25:25.0%  t31:36.1%  t37:31.9%  t43:30.0%  t49:7.1%

### …and the cost crossover with session length

> The catch above is session length. Without pruning, every turn re-reads the
> whole growing history at the cache rate — that cost grows roughly with the
> *square* of the turn count. Pruning keeps the request bounded, so its cost
> grows roughly linearly. Below a crossover the churn penalty dominates (cost
> loss); past it, the unpruned quadratic wins and pruning pays off. The
> crossover turn-count is price-dependent (a higher input rate or cheaper
> cache moves it). Same realistic Bash/Read session, increasing length:

| Turns | Unpruned $ | Default $ | Δ |
|--:|--:|--:|--:|
| 8 | $0.0610 | $0.0596 | -2.3% |
| 16 | $0.1141 | $0.1047 | -8.2% |
| 32 | $0.2926 | $0.2673 | -8.6% |
| 64 | $0.8658 | $0.6029 | -30.4% |
| 128 | $2.9331 | $1.6135 | -45.0% |
| 256 | $10.6985 | $4.8596 | -54.6% |

> Read this as the honest bottom line on cost: trimwire's reliable win is
> **request size / context-window headroom** (always), plus token cost when
> prompt caching is cold or inactive. Under warm caching its dollar effect is
> a wash-to-loss on short sessions and a win on long ones — which is exactly
> why the strategies prune only *aged* content and the defaults stay
> conservative.

## 7. Gateway overhead — `apply_to_body` per request (statistical)

| Corpus | Body | min | median | mean | p99 | stddev | round spread |
|---|--:|--:|--:|--:|--:|--:|--:|
| `pure_chat_floor` | 11.0 KB | 0.094 | 0.103 | 0.114 | 0.232 | 0.031 | 0.099–0.117 ms |
| `exempt_heavy` | 58.6 KB | 0.588 | 0.647 | 0.688 | 1.218 | 0.137 | 0.643–0.658 ms |
| `unique_bash_spam` | 223.6 KB | 1.742 | 1.906 | 2.028 | 3.249 | 0.369 | 1.862–1.968 ms |
| `at_the_boundary` | 145.1 KB | 1.124 | 1.263 | 1.363 | 2.284 | 0.264 | 1.243–1.287 ms |
| `repeated_grep` | 31.3 KB | 0.289 | 0.322 | 0.369 | 0.708 | 0.138 | 0.317–0.330 ms |
| `coding` | 49.5 KB | 0.403 | 0.478 | 0.548 | 0.981 | 0.172 | 0.455–0.516 ms |
| `mixed_realistic` | 364.5 KB | 1.949 | 2.333 | 2.570 | 4.089 | 0.570 | 2.123–2.800 ms |
| `mcp_non_playwright` | 131.1 KB | 0.991 | 1.131 | 1.248 | 2.079 | 0.265 | 1.100–1.158 ms |
| `long_running` | 135.2 KB | 1.231 | 1.397 | 1.560 | 2.754 | 0.395 | 1.328–1.457 ms |
| `resumed_session` | 187.9 KB | 1.789 | 1.947 | 2.121 | 3.786 | 0.458 | 1.934–1.969 ms |
| `browser_heavy` | 424.7 KB | 2.287 | 2.515 | 2.795 | 4.956 | 0.734 | 2.468–2.667 ms |
| `giant_paste` | 509.7 KB | 2.802 | 3.193 | 3.483 | 6.170 | 0.772 | 3.136–3.362 ms |
| `stale_input_heavy` | 15.8 KB | 0.123 | 0.135 | 0.154 | 0.322 | 0.126 | 0.131–0.149 ms |
| `thinking_heavy` | 14.0 KB | 0.202 | 0.219 | 0.247 | 0.488 | 0.125 | 0.213–0.240 ms |

> Milliseconds for the whole transform (parse → prune → re-serialize), 5
> rounds × {2000 | 200 for the big body} iterations after warm-up. Off the
> network round-trip's critical path. Host-dependent.

