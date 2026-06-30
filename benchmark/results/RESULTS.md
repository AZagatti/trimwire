# trimwire benchmark — offline replay

> **TL;DR** (model-free pruning, **`default` profile**) — request size
> **0–99% lighter** by session shape (nothing when there's no redundancy);
> the point is **context-window headroom**, not money; cost is non-monotonic
> (wash-to-loss short, ≈ −55% at 256 turns — §6b computes −54.6%); **sub-2 ms**
> overhead; orphan-free + `system` untouched on every corpus + a 3,000-body
> fuzz. The **`gentle`** profile prunes much less (§2), and the **optional
> summarizer** is a separate mode (off by default). Live `claude -p` numbers:
> [`docs/RESULTS.md`](../../docs/RESULTS.md).

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
| `exempt_heavy` | 55.2% → **55.2%** | 0.0% → **0.0%** |
| `subagent_heavy` | 41.5% → **48.3%** | 0.0% → **0.0%** |
| `read_heavy` | 49.8% → **60.1%** | 0.0% → **0.0%** |
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
| `exempt_heavy` | honest low case: load-bearing authoring content → ~nothing to prune | 57.2 KB | 57.2 KB | 0 B | 0.0% | no-op |
| `subagent_heavy` | #124: old subagent findings (past the 8-turn window) get head+tail-salvaged | 125.5 KB | 107.7 KB | 17.8 KB | 14.2% | pruned |
| `read_heavy` | old reads bloat_capped now Read is age-gated (coverage-gap fix); recent reads intact | 83.8 KB | 56.3 KB | 27.5 KB | 32.8% | pruned |
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

## 2. Profiles — `default` / `gentle` (reduction %)

| Corpus | default (aggressive) | gentle (lightest) |
|---|--:|--:|
| `pure_chat_floor` | 0.0% | 0.0% |
| `exempt_heavy` | 0.0% | 0.0% |
| `subagent_heavy` | 14.2% | 0.0% |
| `read_heavy` | 32.8% | 0.0% |
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
> cache-safe strategies (plus opt-in simhash_dedup, off by default), tight knobs
> (`keep_recent_turns=2`, `bloat 4 KB`, `image keep 1`), a verb-class denylist
> (`*screenshot*`/`*navigate*`/`*click*`/`*browser_act*`/`Grep`),
> and reprune on — cleans hardest while keeping reference-data MCP results.
> **`gentle`** = dedup + failed-input-purge + a *conservative* bloat_cap
> (32 KB / keep 6) + a *conservative* thinking_strip (keep 8) + reprune;
> sliding-window, stale_reads, stale_input_cap, and image-strip off (lightest
> touch, least pruning). Pick with `profile = "…"` in your
> config. Their *cost* behaviour is not what you'd guess — see §5.

## 3. Per-strategy contribution (default config)

> Bytes each strategy removes *on top of the others* (turn it off, see what
> comes back). Unlike measuring each strategy in isolation, these add up
> toward the total instead of double-counting the same bytes twice.

| Corpus | failed_input_purge | stale_input_cap | cross_turn_dedup | stale_reads | simhash_dedup | bloat_cap | sliding_window | image_strip | thinking_strip |
|---|--:|--:|--:|--:|--:|--:|--:|--:|--:|
| `pure_chat_floor` | — | — | — | — | — | — | — | — | — |
| `exempt_heavy` | — | — | — | — | — | — | — | — | — |
| `subagent_heavy` | — | — | — | — | — | 17.8 KB | — | — | — |
| `read_heavy` | — | — | — | — | — | 27.5 KB | — | — | — |
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
| `subagent_heavy` | 100.0% | 76.2% | 100.0% |
| `read_heavy` | 100.0% | 46.6% | 100.0% |
| `unique_bash_spam` | 100.0% | 36.7% | 100.0% |
| `at_the_boundary` | 100.0% | 39.7% | 100.0% |
| `repeated_grep` | 100.0% | 23.9% | 55.8% |
| `coding` | 100.0% | 74.4% | 89.8% |
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
> NOTE: these figures are the **stateless** prune (re-prune from scratch each
> turn). The shipped default runs **reprune** (on by default), which replays
> the prior decisions so the prefix stays byte-identical — far more stable than
> shown here. See §5b for the reprune-on numbers (e.g. unique_bash_spam 36.7%
> stateless → 82.7% with reprune).

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
| `exempt_heavy` | $0.0763 | +0.0% | +0.0% |
| `subagent_heavy` | $0.1658 | +105.8% | +0.0% |
| `read_heavy` | $0.1082 | +84.2% | +0.0% |
| `unique_bash_spam` | $0.2771 | +73.4% | +0.0% |
| `at_the_boundary` | $0.1544 | +71.4% | +0.0% |
| `repeated_grep` | $0.0547 | -4.4% | -17.6% |
| `coding` | $0.0913 | -0.4% | +15.8% |
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
| `exempt_heavy` | $0.0763 → $0.0763 ↓ | 100.0% → **100.0%** |
| `subagent_heavy` | $0.3411 → $0.2237 ↓ | 76.2% → **91.7%** |
| `read_heavy` | $0.1993 → $0.1240 ↓ | 46.6% → **88.9%** |
| `unique_bash_spam` | $0.4805 → $0.3182 ↓ | 36.7% → **82.7%** |
| `at_the_boundary` | $0.2646 → $0.1837 ↓ | 39.7% → **83.4%** |
| `repeated_grep` | $0.0523 → $0.0500 ↓ | 23.9% → **82.6%** |
| `coding` | $0.0909 → $0.0758 ↓ | 74.4% → **87.3%** |
| `mixed_realistic` | $0.4205 → $0.3737 ↓ | 46.2% → **81.9%** |
| `mcp_non_playwright` | $0.2188 → $0.1705 ↓ | 60.8% → **88.9%** |
| `long_running` | $0.1904 → $0.2325 ↑ | 84.0% → **85.2%** |
| `resumed_session` | $0.4221 → $0.4107 ↓ | 87.4% → **90.7%** |
| `browser_heavy` | $0.3368 → $0.3621 ↑ | 1.2% → **66.7%** |
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
| `pure_chat_floor` | 11.0 KB | 0.015 | 0.017 | 0.020 | 0.056 | 0.011 | 0.016–0.022 ms |
| `exempt_heavy` | 58.8 KB | 0.109 | 0.131 | 0.150 | 0.337 | 0.062 | 0.128–0.134 ms |
| `subagent_heavy` | 127.0 KB | 0.255 | 0.307 | 0.343 | 0.694 | 0.104 | 0.304–0.315 ms |
| `read_heavy` | 85.3 KB | 0.242 | 0.293 | 0.329 | 0.637 | 0.093 | 0.286–0.304 ms |
| `unique_bash_spam` | 223.6 KB | 0.475 | 0.552 | 0.610 | 1.287 | 0.168 | 0.544–0.570 ms |
| `at_the_boundary` | 145.1 KB | 0.274 | 0.332 | 0.369 | 0.736 | 0.101 | 0.330–0.336 ms |
| `repeated_grep` | 31.3 KB | 0.140 | 0.170 | 0.192 | 0.401 | 0.060 | 0.166–0.172 ms |
| `coding` | 49.5 KB | 0.224 | 0.271 | 0.302 | 0.591 | 0.080 | 0.268–0.283 ms |
| `mixed_realistic` | 364.5 KB | 0.921 | 1.101 | 1.203 | 2.243 | 0.281 | 1.091–1.111 ms |
| `mcp_non_playwright` | 131.1 KB | 0.292 | 0.355 | 0.389 | 0.721 | 0.098 | 0.351–0.357 ms |
| `long_running` | 135.2 KB | 0.399 | 0.461 | 0.500 | 0.923 | 0.111 | 0.457–0.467 ms |
| `resumed_session` | 187.9 KB | 0.777 | 0.902 | 0.971 | 1.735 | 0.205 | 0.893–0.913 ms |
| `browser_heavy` | 424.7 KB | 0.763 | 0.875 | 0.929 | 1.551 | 0.166 | 0.863–0.889 ms |
| `giant_paste` | 509.7 KB | 0.821 | 0.986 | 1.042 | 1.813 | 0.195 | 0.968–0.999 ms |
| `stale_input_heavy` | 15.8 KB | 0.062 | 0.072 | 0.083 | 0.196 | 0.040 | 0.071–0.073 ms |
| `thinking_heavy` | 14.0 KB | 0.070 | 0.081 | 0.091 | 0.205 | 0.034 | 0.080–0.084 ms |

> Milliseconds for the whole transform (parse → prune → re-serialize), 5
> rounds × {2000 | 200 for the big body} iterations after warm-up. Off the
> network round-trip's critical path. Host-dependent.

