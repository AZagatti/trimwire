"""Unit tests for ai_review.py's tolerant JSON parsing + salvage.

These guard the highest-risk logic in the review engine: if parse_json silently
drops findings when a model truncates or emits malformed JSON, a real review would
lose issues without anyone noticing. Stdlib only (matches ai_review.py).

Run:  python3 scripts/test_ai_review.py      (or: pytest scripts/test_ai_review.py)
"""
import os
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
