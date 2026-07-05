"""Unit tests for ai_review.py's tolerant JSON parsing + salvage.

These guard the highest-risk logic in the review engine: if parse_json silently
drops findings when a model truncates or emits malformed JSON, a real review would
lose issues without anyone noticing. Stdlib only (matches ai_review.py).

Run:  python3 scripts/test_ai_review.py      (or: pytest scripts/test_ai_review.py)
"""
import json
import os
import re
import sys
import unittest
from unittest import mock
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import ai_review as R  # noqa: E402
import ai_review_personas as PZ  # noqa: E402


class TestParseJson(unittest.TestCase):
    def test_happy_path(self):
        o = R.parse_json('{"verdict":"comment","findings":[{"severity":"bug","title":"x"}]}')
        self.assertEqual(o["verdict"], "comment")
        self.assertEqual(len(o["findings"]), 1)
        self.assertNotIn("_salvaged", o)

    def test_fenced(self):
        o = R.parse_json('```json\n{"verdict":"approve","findings":[]}\n```')
        self.assertEqual(o["verdict"], "approve")

    def test_prose_after_closing_fence(self):
        obj = R.parse_json('```json\n{"verdict": "approve", "findings": []}\n```\nThanks!')
        self.assertEqual(obj["verdict"], "approve")

    def test_fenced_with_inner_code_block_in_detail(self):
        # A fenced response whose `detail` contains an inner ```code``` block with real
        # newlines must NOT be truncated at that inner fence (regression: greedy DOTALL strip
        # dropped every finding). The closing fence is the LAST ```, not the first.
        raw = ('```json\n{"findings": [{"severity": "bug", "file": "a.rs", "line": 5, '
               '"title": "unwrap panic", "detail": "call site:\n```rust\nfoo.unwrap()\n```\n'
               'panics"}]}\n```')
        obj = R.parse_json(raw)
        self.assertEqual(len(obj["findings"]), 1)
        self.assertEqual(obj["findings"][0]["title"], "unwrap panic")

    def test_prose_wrapped(self):
        o = R.parse_json('Here is my review:\n{"verdict":"approve","findings":[]}\nThanks!')
        self.assertEqual(o["verdict"], "approve")

    def test_brace_inside_string(self):
        # the old brace-counter miscounted braces inside string literals
        o = R.parse_json('{"verdict":"comment","findings":'
                         '[{"severity":"bug","title":"uses HashMap<K,{V}>","line":2}]}')
        self.assertEqual(o["findings"][0]["title"], "uses HashMap<K,{V}>")

    def test_truncated_salvages_complete_findings(self):
        # root object never closes; the first finding is complete, the second is cut off
        trunc = ('{"verdict":"request_changes","summary":"...","findings":['
                 '{"severity":"security","title":"token leak","file":"x.rs","line":5},'
                 '{"severity":"bug","title":"panic on unwrap","file":"y.rs","line":9,"detail":"oo')
        o = R.parse_json(trunc)
        self.assertTrue(o.get("_salvaged"))
        self.assertEqual(o["verdict"], "request_changes")
        self.assertEqual([f["title"] for f in o["findings"]], ["token leak"])

    def test_control_char_salvage(self):
        bad = ('{"verdict":"comment","findings":'
               '[{"severity":"bug","title":"bad","detail":"line1\x01line2"}]}')
        o = R.parse_json(bad)
        self.assertEqual(len(o["findings"]), 1)

    def test_bare_finding_object_salvaged(self):
        # a lone finding-like object (no verdict/findings wrapper) is recovered as a
        # finding rather than dropped — better than losing a real issue
        o = R.parse_json('{"severity":"bug","title":"x"}')
        self.assertTrue(o.get("_salvaged"))
        self.assertEqual(len(o["findings"]), 1)

    def test_unrecoverable_object_raises(self):
        # a dict with neither review keys nor any finding has nothing to salvage
        with self.assertRaises(ValueError):
            R.parse_json('{"foo":"bar","baz":1}')

    def test_unrecoverable_text_raises(self):
        with self.assertRaises(ValueError):
            R.parse_json("this is not json at all")


class TestIterJsonObjects(unittest.TestCase):
    def test_yields_top_level_and_nested(self):
        text = '{"a":{"severity":"bug","title":"t"}}'
        objs = list(R._iter_json_objects(text))
        self.assertEqual(objs[0], text)          # outer first
        self.assertIn('{"severity":"bug","title":"t"}', objs)  # nested too

    def test_ignores_braces_in_strings(self):
        objs = list(R._iter_json_objects('{"k":"a}b{c"}'))
        self.assertEqual(objs[0], '{"k":"a}b{c"}')

    def test_first_json_object(self):
        self.assertEqual(R._first_json_object('x {"a":1} y'), '{"a":1}')
        with self.assertRaises(ValueError):
            R._first_json_object("no object here")


class TestMdSafe(unittest.TestCase):
    """_md_safe guards the rendered comment against layout-break / markdown injection
    from attacker-influenced model output — security-relevant, so it gets tests."""
    def test_escapes_details_and_marker(self):
        s = R._md_safe("evil </details> and <!-- forge marker -->")
        self.assertNotIn("</details>", s)
        self.assertNotIn("<!--", s)

    def test_defangs_links(self):
        s = R._md_safe("see [click me](https://evil.example)")
        self.assertNotIn("](https", s)  # zero-width space inserted between ] and (

    def test_defangs_html_links(self):
        s = R._md_safe('phish <a href="https://evil.example">login</a> now')
        self.assertNotIn("<a href", s)   # raw HTML anchor neutralized (GitHub renders it)

    def test_defangs_triple_backtick_fence(self):
        s = R._md_safe("escape ```json\nmalicious markdown\n``` after")
        self.assertNotIn("```", s)       # no run of 3+ raw backticks survives to break a fence

    def test_keeps_inline_code_backticks(self):
        s = R._md_safe("use the `foo` and ``bar`` helpers")
        self.assertIn("`foo`", s)        # single/double backticks (inline code) are untouched

    def test_non_string_coerced(self):
        self.assertEqual(R._md_safe(None), "None")
        self.assertEqual(R._md_safe(42), "42")


class TestBuildDiff(unittest.TestCase):
    def test_truncates_large_patch_with_marker(self):
        files = [{"filename": "src/a.rs", "patch": "line\n" * 20000, "additions": 1}]
        text, kept, total = R.build_diff(files)
        self.assertIn("NOT a code defect", text)  # graceful truncation marker
        self.assertEqual((kept, total), (1, 1))

    def test_skip_re_top_level_dist(self):
        self.assertIsNotNone(R.SKIP_RE.search("dist/bundle.js"))       # top-level dist/
        self.assertIsNotNone(R.SKIP_RE.search("site/dist/bundle.js"))  # nested dist/
        self.assertIsNone(R.SKIP_RE.search("src/distance.rs"))         # 'dist' prefix isn't dist/

    def test_risk_ordered_small_security_file_beats_large_safe_file(self):
        files = [
            {"filename": "src/big_ui.rs", "patch": "@@\n" + "+ui line\n" * 200, "additions": 200},
            {"filename": "src/auth/token.rs", "patch": "@@\n+let k = api_key;", "additions": 2},
        ]
        text, kept, total = R.build_diff(files)
        self.assertEqual((kept, total), (2, 2))
        # the tiny auth file must appear BEFORE the large UI file in the sent diff
        self.assertLess(text.index("src/auth/token.rs"), text.index("src/big_ui.rs"))

    def test_risk_content_lifts_unsafe_block(self):
        files = [
            {"filename": "src/render.rs", "patch": "@@\n+draw();", "additions": 1},
            {"filename": "src/mem.rs", "patch": "@@\n+unsafe { ptr::write(p, v); }", "additions": 1},
        ]
        text, _, _ = R.build_diff(files)
        self.assertLess(text.index("src/mem.rs"), text.index("src/render.rs"))

    def test_test_files_sink_below_code(self):
        files = [
            {"filename": "tests/big_test.rs", "patch": "@@\n" + "+assert!(x)\n" * 50, "additions": 50},
            {"filename": "src/logic.rs", "patch": "@@\n+let y = compute();", "additions": 1},
        ]
        text, _, _ = R.build_diff(files)
        self.assertLess(text.index("src/logic.rs"), text.index("tests/big_test.rs"))

    def test_skips_lockfiles_and_removed(self):
        files = [
            {"filename": "Cargo.lock", "patch": "huge", "additions": 5},
            {"filename": "src/b.rs", "patch": "@@ real code", "status": "modified"},
            {"filename": "src/c.rs", "patch": "@@ gone", "status": "removed"},
        ]
        text, kept, total = R.build_diff(files)
        self.assertEqual((kept, total), (1, 3))       # only src/b.rs survives
        self.assertIn("src/b.rs", text)
        self.assertNotIn("Cargo.lock", text)


class TestAggregatorPrompt(unittest.TestCase):
    def test_default_aggregator_requests_rule_suggestions(self):
        # render()/main() read agg["rule_suggestions"]; the fallback prompt must ask for it
        self.assertIn("rule_suggestions", R._DEFAULT_AGGREGATOR)
        self.assertIn("verdict", R._DEFAULT_AGGREGATOR)


class TestConfigValidation(unittest.TestCase):
    # importing ai_review at the top of this file already proves import doesn't exit
    def test_default_config_is_valid(self):
        R._validate_config()  # default PANEL/AGGREGATOR must not raise

    def test_rejects_unknown_provider(self):
        with self.assertRaises(SystemExit):
            R._validate_member({"name": "x", "provider": "nope", "model": "y"}, "t")

    def test_rejects_missing_keys(self):
        with self.assertRaises(SystemExit):
            R._validate_member({"name": "x"}, "t")


class TestPersonas(unittest.TestCase):
    def test_routing_rust(self):
        names = {m["name"] for m in PZ.relevant_modules(["src/x.rs", "Cargo.toml"])}
        self.assertIn("SENTINEL", names)   # always-on baseline
        self.assertIn("FERRUS", names)     # src/**/*.rs
        self.assertIn("SENTRY", names)     # Cargo.toml -> deps glob

    def test_docs_only_drops_code_personas(self):
        names = {m["name"] for m in PZ.relevant_modules(["README.md"])}
        self.assertNotIn("SENTINEL", names)  # needs_code + no code file changed
        self.assertNotIn("WARDEN", names)
        self.assertIn("SCRIBE", names)

    def test_group_by_model_merges_same_lane(self):
        mods = [m for m in PZ.MODULES if m["name"] in ("SENTINEL", "FERRUS")]  # both GLM
        self.assertEqual(len(PZ.group_by_model(mods)), 1)

    def test_system_prompt_has_reasoning_example_and_anchor(self):
        # The bench-winning config (N=4): think-first _reasoning + one worked example (schema) +
        # per-checklist "return empty" anchor. Lock all three into the composed system prompt.
        sysp = PZ.build_system([m for m in PZ.MODULES if m["name"] == "SENTINEL"])
        self.assertIn("_reasoning", sysp)                                   # P1 think-first
        self.assertIn("Example of a well-formed finding", sysp)             # P5 worked example
        self.assertIn("return an empty findings array for this perspective", sysp)  # P4 anchor
        # dev-notes kept (P3 reverted — removing them measured net-negative)
        gk = PZ.build_system([m for m in PZ.MODULES if m["name"] == "GATEKEEPER"])
        self.assertIn("LOW bench evidence", gk)

    def test_gatekeeper_scoped_to_gha_workflows(self):
        # GATEKEEPER's checklist is GHA-workflow-specific; it must fire on workflow YAML but
        # NOT on generic project YAML (docker-compose / k8s), which used to burn a call + noise.
        self.assertIn("GATEKEEPER",
                      {m["name"] for m in PZ.relevant_modules([".github/workflows/ci.yml"])})
        self.assertNotIn("GATEKEEPER",
                         {m["name"] for m in PZ.relevant_modules(["docker-compose.yml"])})
        self.assertNotIn("GATEKEEPER",
                         {m["name"] for m in PZ.relevant_modules(["k8s/deployment.yaml"])})

    def test_surveyor_routes_on_code_not_docs(self):
        # SURVEYOR (coverage enumeration) fires on any code PR, not on docs-only.
        self.assertIn("SURVEYOR", {m["name"] for m in PZ.relevant_modules(["src/x.rs"])})
        self.assertIn("SURVEYOR", {m["name"] for m in PZ.relevant_modules(["scripts/a.py"])})
        self.assertNotIn("SURVEYOR", {m["name"] for m in PZ.relevant_modules(["README.md"])})

    def test_surveyor_runs_solo_never_paired(self):
        # A `solo` module must be its own single-module call, never grouped with another persona,
        # even though it shares the GLM lane with SENTINEL/FERRUS/SCOUT.
        mods = PZ.relevant_modules(["src/x.rs", "tests/a.rs", "Cargo.toml"])
        groups = PZ.group_correlated_pairs(mods)
        surv = [g for g in groups if any(m["name"] == "SURVEYOR" for m in g)]
        self.assertEqual(len(surv), 1)
        self.assertEqual([m["name"] for m in surv[0]], ["SURVEYOR"])  # alone
        # solo-extraction must not drop or duplicate any module
        self.assertEqual(sorted(m["name"] for g in groups for m in g),
                         sorted(m["name"] for m in mods))
        self.assertTrue(all(len(g) <= 2 for g in groups))

    def test_aggregate_dedups_identical(self):
        d = {"file": "a.rs", "line": 1, "title": "same bug", "severity": "bug"}
        out, stats = PZ.aggregate([dict(d), dict(d), dict(d)])
        self.assertEqual(len(out), 1)
        self.assertEqual(stats["raw"], 3)

    def test_aggregate_promotes_higher_severity_on_title_collision(self):
        # low severity inserted FIRST — must not mask the later security finding
        lo = {"file": "a.rs", "line": 1, "title": "Same Issue", "severity": "suggestion", "detail": "short"}
        hi = {"file": "a.rs", "line": 1, "title": "same issue", "severity": "security", "detail": "much longer detail"}
        out, _ = PZ.aggregate([lo, hi])
        self.assertEqual(len(out), 1)                       # normalized titles collapse
        self.assertEqual(out[0]["severity"], "security")    # promoted, not first-wins
        self.assertEqual(out[0]["detail"], "much longer detail")  # richest detail kept
        self.assertEqual(out[0]["consensus"], 2)

    def test_aggregate_higher_severity_survives_when_first(self):
        hi = {"file": "a.rs", "line": 1, "title": "same", "severity": "bug"}
        lo = {"file": "a.rs", "line": 1, "title": "same", "severity": "question"}
        out, _ = PZ.aggregate([hi, lo])                     # high severity inserted first
        self.assertEqual(out[0]["severity"], "bug")

    def test_aggregate_keeps_distinct_same_line_findings(self):
        # two genuinely different bugs on the same line must both survive
        f1 = {"file": "a.rs", "line": 5, "title": "unwrap panic", "severity": "bug"}
        f2 = {"file": "a.rs", "line": 5, "title": "auth bypass", "severity": "security"}
        out, _ = PZ.aggregate([f1, f2])
        self.assertEqual(len(out), 2)
        self.assertEqual({x["title"] for x in out}, {"unwrap panic", "auth bypass"})

    def test_near_dup_merges_reworded_subset(self):
        # same bug, one title a verbose restatement of the other -> merge (the cross-persona case)
        a = {"file": "ci.yml", "line": 3, "title": "Action pinned by mutable v6 tag", "severity": "security", "persona": "GATEKEEPER"}
        b = {"file": "ci.yml", "line": 3, "title": "Third-party action pinned by mutable v6 tag", "severity": "security", "persona": "SENTRY"}
        out, _ = PZ.aggregate([a, b])
        self.assertEqual(len(out), 1)
        self.assertEqual(out[0]["consensus"], 2)
        self.assertEqual(set(out[0]["personas"]), {"GATEKEEPER", "SENTRY"})

    def test_near_dup_keeps_distinct_bugs(self):
        a = {"file": "a.rs", "line": 5, "title": "unwrap panics on empty input", "severity": "bug"}
        b = {"file": "a.rs", "line": 5, "title": "data race on static counter", "severity": "bug"}
        out, _ = PZ.aggregate([a, b])
        self.assertEqual(len(out), 2)                    # unrelated titles -> both kept

    def test_near_dup_keeps_send_vs_sync(self):
        # subtle-but-distinct: Send vs Sync differ by one key word -> must NOT merge
        a = {"file": "a.rs", "line": 1, "title": "Send bound insufficient here", "severity": "bug"}
        b = {"file": "a.rs", "line": 1, "title": "Sync bound needed here", "severity": "bug"}
        out, _ = PZ.aggregate([a, b])
        self.assertEqual(len(out), 2)

    def test_near_dup_respects_line_distance(self):
        a = {"file": "a.rs", "line": 5, "title": "missing bounds check on index", "severity": "bug"}
        b = {"file": "a.rs", "line": 80, "title": "missing bounds check on index", "severity": "bug"}
        out, _ = PZ.aggregate([a, b])
        self.assertEqual(len(out), 2)                    # same title but far apart -> different issue

    def test_aggregate_uses_consensus_not_dupes(self):
        d = {"file": "a.rs", "line": 1, "title": "t", "severity": "bug", "persona": "SENTINEL"}
        e = {"file": "a.rs", "line": 1, "title": "t", "severity": "bug", "persona": "WARDEN"}
        out, _ = PZ.aggregate([d, e])
        self.assertEqual(out[0]["consensus"], 2)            # render() reads .consensus for the badge
        self.assertNotIn("_dupes", out[0])
        self.assertEqual(set(out[0]["personas"]), {"SENTINEL", "WARDEN"})

    def test_build_system_has_coverage_and_schema(self):
        s = PZ.build_system([m for m in PZ.MODULES if m["name"] == "ARGUS"])
        self.assertIn("COVERAGE", s)     # coverage directive baked in
        self.assertIn("findings", s)     # output schema
        self.assertIn("ARGUS", s)        # persona checklist composed in

    def test_shared_preamble_has_intent_directive(self):
        self.assertIn("intent-vs-implementation", PZ.SHARED_PREAMBLE)

    def test_ferrus_checklist_covers_benchmark_misses(self):
        ferrus = next(m for m in PZ.MODULES if m["name"] == "FERRUS")["checklist"]
        self.assertIn("non_exhaustive", ferrus)          # the bug that broke our own build
        self.assertIn("Send ≠ Sync", ferrus)             # wrong-direction impl
        self.assertIn("read-modify-write", ferrus)       # non-atomic RMW data race

    def test_sentry_checklist_enumerates_every_ref(self):
        sentry = next(m for m in PZ.MODULES if m["name"] == "SENTRY")["checklist"]
        self.assertIn("EVERY", sentry)                   # enumerate-each directive


    def test_svelte5_cheatsheet_present_and_current(self):
        cs = Path(__file__).resolve().parent.parent / ".github" / "ai-review" / "cheatsheets" / "svelte5.md"
        self.assertTrue(cs.exists(), "svelte5 cheatsheet must be committed")
        txt = cs.read_text()
        self.assertIn("$props()", txt)   # current Svelte-5 fact
        self.assertIn("onclick", txt)    # not on:click


class TestLegacyRouting(unittest.TestCase):
    """The manual workflow sets AI_REVIEW_PANEL/_AGGREGATOR from user-entered models;
    those are only honored on the legacy path, so an explicit value must force it."""
    def _clear(self):
        for k in ("AI_REVIEW_LEGACY_PANEL", "AI_REVIEW_PANEL", "AI_REVIEW_AGGREGATOR"):
            os.environ.pop(k, None)

    def test_explicit_flag_selects_legacy(self):
        with mock.patch.dict(os.environ, {}, clear=False):
            self._clear()
            os.environ["AI_REVIEW_LEGACY_PANEL"] = "1"
            self.assertTrue(R._use_legacy_panel())

    def test_panel_env_forces_legacy(self):
        with mock.patch.dict(os.environ, {}, clear=False):
            self._clear()
            os.environ["AI_REVIEW_PANEL"] = '[{"name":"x","provider":"openrouter","model":"x/y"}]'
            self.assertTrue(R._use_legacy_panel())  # manual selection must not be discarded

    def test_aggregator_env_forces_legacy(self):
        with mock.patch.dict(os.environ, {}, clear=False):
            self._clear()
            os.environ["AI_REVIEW_AGGREGATOR"] = '{"name":"x","provider":"openrouter","model":"x/y"}'
            self.assertTrue(R._use_legacy_panel())

    def test_default_is_persona_path(self):
        with mock.patch.dict(os.environ, {}, clear=False):
            self._clear()
            self.assertFalse(R._use_legacy_panel())


class TestRunPersonasDispatch(unittest.TestCase):
    """run_personas dispatches CORRELATED PERSONA PAIRS (≤2/call), not one-per-persona and not
    one-composed-per-lane — the bench sweet spot. Mock chat so no API is hit."""
    def _fake_chat(self, calls):
        def chat(provider, model, system, user, max_tokens=4000, extra=None):
            names = re.findall(r"Perspective: (\w+)", system)   # a call may carry 1-2 personas
            calls.append(names)
            return json.dumps({"verdict": "comment", "findings": [
                {"severity": "bug", "title": f"issue-{names[0]}", "file": "src/x.rs",
                 "line": 1, "persona": names[0]}]})
        return chat

    def test_calls_are_correlated_pairs(self):
        files = [{"filename": "src/x.rs", "patch": "@@\n+unsafe { foo(); }", "additions": 1}]
        mods = PZ.relevant_modules(["src/x.rs"])
        self.assertGreater(len(mods), 1)
        calls = []
        with mock.patch.dict(os.environ, {}, clear=False), \
                mock.patch.object(R, "chat", self._fake_chat(calls)):
            os.environ.pop("AI_REVIEW_SAMPLES", None)
            agg, panel = R.run_personas(files, "u", "reviewer", "")
        # fewer calls than personas (some paired), each call carries at most 2, all covered once
        self.assertLessEqual(len(calls), len(mods))
        self.assertTrue(all(1 <= len(c) <= 2 for c in calls))
        self.assertEqual(sorted(n for c in calls for n in c), sorted(m["name"] for m in mods))
        self.assertTrue(agg["findings"])


class TestMultiSample(unittest.TestCase):
    def _count(self):
        with mock.patch.dict(os.environ, {}, clear=False):
            os.environ.pop("AI_REVIEW_SAMPLES", None)
            return R._sample_count()

    def test_default_is_single_sample(self):
        self.assertEqual(self._count(), 1)

    def test_env_sets_sample_count_clamped(self):
        with mock.patch.dict(os.environ, {"AI_REVIEW_SAMPLES": "3"}, clear=False):
            self.assertEqual(R._sample_count(), 3)
        with mock.patch.dict(os.environ, {"AI_REVIEW_SAMPLES": "99"}, clear=False):
            self.assertEqual(R._sample_count(), len(R._TEMP_BANDS))   # clamped to band count
        with mock.patch.dict(os.environ, {"AI_REVIEW_SAMPLES": "bogus"}, clear=False):
            self.assertEqual(R._sample_count(), 1)                    # non-int -> safe default

    def test_temperature_bands_anchor_first(self):
        self.assertEqual(R._temperature_bands(1), [0.1])             # single = deterministic anchor
        b = R._temperature_bands(3)
        self.assertEqual(len(b), 3)
        self.assertEqual(b[0], 0.1)                                   # first pass is always the anchor
        self.assertGreater(max(b[1:]), 0.1)                          # later passes add diversity


class TestReplacementSanitization(unittest.TestCase):
    """findings.json feeds ```suggestion blocks in inline comments — a run of
    backticks in the model's replacement would break out of the fence."""
    def test_defangs_fence_breakout(self):
        out = R._safe_replacement("let x = 1;\n```\nmalicious markdown")
        self.assertNotIn("```", out)

    def test_none_for_empty(self):
        self.assertIsNone(R._safe_replacement(""))
        self.assertIsNone(R._safe_replacement(None))
        self.assertIsNone(R._safe_replacement("   "))

    def test_keeps_normal_code_verbatim(self):
        self.assertEqual(R._safe_replacement("let x = foo(1);"), "let x = foo(1);")


class TestStrictMode(unittest.TestCase):
    """Label 'ai-review-strict' -> meta.strict -> render shows only consensus>=2
    findings, but never hides security."""
    _PANEL = [{"name": "X", "model": "x/y", "ok": True, "review": {}}]

    def _agg(self):
        return {"verdict": "comment", "findings": [
            {"severity": "bug", "title": "solo bug", "file": "a.rs", "line": 1, "consensus": 1},
            {"severity": "bug", "title": "agreed bug", "file": "b.rs", "line": 2, "consensus": 2},
            {"severity": "security", "title": "solo sec", "file": "c.rs", "line": 3, "consensus": 1},
        ]}

    def test_strict_hides_solo_nonsecurity(self):
        out = R.render({"strict": True}, self._agg(), self._PANEL, 3, 3)
        self.assertNotIn("solo bug", out)      # consensus 1, non-security -> hidden
        self.assertIn("agreed bug", out)       # consensus 2 -> shown
        self.assertIn("solo sec", out)         # security always shown
        self.assertIn("strict mode", out)

    def test_non_strict_shows_everything(self):
        out = R.render({}, self._agg(), self._PANEL, 3, 3)
        self.assertIn("solo bug", out)
        self.assertIn("agreed bug", out)
        self.assertNotIn("strict mode", out)


class TestCrossFileCollapse(unittest.TestCase):
    def test_same_issue_across_files_collapses_in_summary(self):
        # a persona flagging the identical issue in 3 files -> ONE summary entry, +2 more
        findings = [
            {"severity": "security", "title": "Unpinned action tag", "file": "a.yml", "line": 3},
            {"severity": "security", "title": "Unpinned action tag", "file": "b.yml", "line": 5},
            {"severity": "security", "title": "unpinned action tag", "file": "c.yml", "line": 7},
            {"severity": "bug", "title": "Different real bug", "file": "d.rs", "line": 1},
        ]
        out = R.render({}, {"findings": findings, "verdict": "comment"},
                       [{"name": "X", "model": "x/y", "ok": True, "review": {}}], 4, 4)
        # the repeated issue shows once with a "+2 more file(s)" note, not 3 times
        self.assertEqual(out.count("Unpinned action tag"), 1)
        self.assertIn("+2 more file(s)", out)
        self.assertIn("Different real bug", out)          # distinct bug still shown

    def test_collapse_keeps_highest_severity(self):
        fs = [{"severity": "suggestion", "title": "same", "file": "a.rs", "line": 1},
              {"severity": "security", "title": "same", "file": "b.rs", "line": 1}]
        out = R._collapse_repeats(fs)
        self.assertEqual(len(out), 1)
        self.assertEqual(out[0]["severity"], "security")
        self.assertEqual(len(out[0]["_locs"]), 2)


class TestRenderSafety(unittest.TestCase):
    def test_raw_panel_backticks_dont_break_fence(self):
        panel = [{"name": "X", "model": "x/y", "ok": True,
                  "review": {"findings": [{"detail": "escape ```fence``` here"}]}}]
        out = R.render({}, {"findings": [], "verdict": "approve"}, panel, 1, 1)
        idx = out.find("```json")
        self.assertGreater(idx, -1)
        # content between the opening ```json and its intended closing \n``` must be fence-free
        body, _sep, _rest = out[idx + len("```json"):].partition("\n```")
        self.assertNotIn("```", body)


class TestDiffSymbols(unittest.TestCase):
    """_diff_symbols feeds cross-file grep: added/changed symbols (verify callers) vs
    removed-only symbols (external references likely broken)."""
    def test_added_symbol_detected(self):
        files = [{"filename": "src/a.rs", "patch": "@@\n+pub fn process_request() {}"}]
        added, removed_only = R._diff_symbols(files)
        self.assertIn("process_request", added)
        self.assertEqual(removed_only, [])

    def test_removed_only_symbol_detected(self):
        files = [{"filename": "src/a.rs", "patch": "@@\n-pub fn old_helper() {}"}]
        added, removed_only = R._diff_symbols(files)
        self.assertIn("old_helper", removed_only)
        self.assertNotIn("old_helper", added)

    def test_signature_change_is_not_removed_only(self):
        # same name on '-' and '+' == modification, not a deletion → stays out of removed_only
        files = [{"filename": "src/a.rs",
                  "patch": "@@\n-pub fn handle(a: u8) {}\n+pub fn handle(a: u8, b: u8) {}"}]
        added, removed_only = R._diff_symbols(files)
        self.assertIn("handle", added)
        self.assertEqual(removed_only, [])

    def test_ignores_commented_out_def(self):
        # a commented-out fn must not be mistaken for a real definition (no false cross-file grep)
        files = [{"filename": "src/a.rs",
                  "patch": "@@\n+// pub fn ghost_fn() {}\n+let x = 1; /* fn also_ghost */\n+pub fn real_fn() {}"}]
        added, _ = R._diff_symbols(files)
        self.assertIn("real_fn", added)
        self.assertNotIn("ghost_fn", added)
        self.assertNotIn("also_ghost", added)

    def test_trailing_comment_after_real_def_still_detected(self):
        files = [{"filename": "src/a.rs", "patch": "@@\n+pub fn keeper() { // TODO tidy\n"}]
        added, _ = R._diff_symbols(files)
        self.assertIn("keeper", added)

    def test_git_exemplar_empty_for_unsafe_or_missing_paths(self):
        # paths with shell/glob metacharacters are filtered out -> no git call, empty result
        self.assertEqual(R.git_history_exemplar([{"filename": "a; rm -rf /.rs"}]), "")
        self.assertEqual(R.git_history_exemplar([{"filename": ""}]), "")
        self.assertEqual(R.git_history_exemplar([]), "")


class TestCiSignals(unittest.TestCase):
    def _with_artifacts(self, files):
        import tempfile
        import pathlib
        d = pathlib.Path(tempfile.mkdtemp())
        for name, txt in files.items():
            (d / name).write_text(txt)
        old = R.ARTIFACTS
        R.ARTIFACTS = d
        try:
            return R.read_ci_signals()
        finally:
            R.ARTIFACTS = old

    def test_injects_conclusions_and_failure_logs(self):
        out = self._with_artifacts({
            "ci-signals.txt": "clippy: failure\ntests: success",
            "ci-failure-logs.txt": "error[E0308] --> src/x.rs:42",
        })
        self.assertIn("clippy: failure", out)
        self.assertIn("E0308", out)          # failure log line injected
        self.assertIn("untrusted", out)      # security note present

    def test_empty_when_no_ci_artifacts(self):
        self.assertEqual(self._with_artifacts({}), "")


if __name__ == "__main__":
    unittest.main(verbosity=2)
