"""Unit tests for ai_review.py's tolerant JSON parsing + salvage.

These guard the highest-risk logic in the review engine: if parse_json silently
drops findings when a model truncates or emits malformed JSON, a real review would
lose issues without anyone noticing. Stdlib only (matches ai_review.py).

Run:  python3 scripts/test_ai_review.py      (or: pytest scripts/test_ai_review.py)
"""
import sys
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import ai_review as R  # noqa: E402


class TestParseJson(unittest.TestCase):
    def test_happy_path(self):
        o = R.parse_json('{"verdict":"comment","findings":[{"severity":"bug","title":"x"}]}')
        self.assertEqual(o["verdict"], "comment")
        self.assertEqual(len(o["findings"]), 1)
        self.assertNotIn("_salvaged", o)

    def test_fenced(self):
        o = R.parse_json('```json\n{"verdict":"approve","findings":[]}\n```')
        self.assertEqual(o["verdict"], "approve")

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


if __name__ == "__main__":
    unittest.main(verbosity=2)
