"""Unit tests for ai_review_verify.apply_replacements — the pure patch-apply core of
the manual-flow compile-verifier. Stdlib only; no git/cargo needed."""
import sys
import tempfile
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import ai_review_verify as V  # noqa: E402


class TestApplyReplacements(unittest.TestCase):
    def _root(self, files: dict) -> Path:
        d = Path(tempfile.mkdtemp())
        for name, txt in files.items():
            p = d / name
            p.parent.mkdir(parents=True, exist_ok=True)
            p.write_text(txt, encoding="utf-8")
        return d

    def test_applies_single_line_replacement(self):
        root = self._root({"src/a.rs": "line1\nlet x = old();\nline3\n"})
        applied, skipped = V.apply_replacements(
            [{"file": "src/a.rs", "line": 2, "replacement": "let x = new();"}], root)
        self.assertEqual(len(applied), 1)
        self.assertEqual(skipped, [])
        self.assertEqual((root / "src/a.rs").read_text(), "line1\nlet x = new();\nline3\n")

    def test_multiple_edits_same_file_apply_highest_line_first(self):
        # if applied low-first, editing line 2 would shift line 4; highest-first keeps anchors valid
        root = self._root({"a.rs": "1\n2\n3\n4\n5\n"})
        applied, _ = V.apply_replacements([
            {"file": "a.rs", "line": 2, "replacement": "TWO"},
            {"file": "a.rs", "line": 4, "replacement": "FOUR"},
        ], root)
        self.assertEqual(len(applied), 2)
        self.assertEqual((root / "a.rs").read_text(), "1\nTWO\n3\nFOUR\n5\n")

    def test_skips_multiline_replacement(self):
        root = self._root({"a.rs": "x\n"})
        applied, skipped = V.apply_replacements(
            [{"file": "a.rs", "line": 1, "replacement": "line one\nline two"}], root)
        self.assertEqual(applied, [])
        self.assertEqual(skipped[0]["reason"], "multi-line replacement")

    def test_skips_missing_file(self):
        root = self._root({})
        applied, skipped = V.apply_replacements(
            [{"file": "nope.rs", "line": 1, "replacement": "x"}], root)
        self.assertEqual(applied, [])
        self.assertEqual(skipped[0]["reason"], "file not found")

    def test_skips_line_out_of_range(self):
        root = self._root({"a.rs": "only one line\n"})
        applied, skipped = V.apply_replacements(
            [{"file": "a.rs", "line": 99, "replacement": "x"}], root)
        self.assertEqual(applied, [])
        self.assertEqual(skipped[0]["reason"], "line out of range")

    def test_path_escape_is_blocked(self):
        root = self._root({"a.rs": "x\n"})
        applied, skipped = V.apply_replacements(
            [{"file": "../../etc/passwd", "line": 1, "replacement": "pwned"}], root)
        self.assertEqual(applied, [])
        self.assertEqual(skipped[0]["reason"], "path escapes root")

    def test_ignores_findings_without_replacement(self):
        root = self._root({"a.rs": "x\n"})
        applied, skipped = V.apply_replacements(
            [{"file": "a.rs", "line": 1, "title": "no fix here"}], root)
        self.assertEqual((applied, skipped), ([], []))

    def test_bool_line_is_not_treated_as_int(self):
        root = self._root({"a.rs": "x\n"})
        applied, _ = V.apply_replacements(
            [{"file": "a.rs", "line": True, "replacement": "y"}], root)
        self.assertEqual(applied, [])

    def test_summarize_flags_rust(self):
        s = V.summarize([{"file": "src/a.rs", "line": 1}, {"file": "b.py", "line": 2}], [])
        self.assertEqual(s["files"], ["b.py", "src/a.rs"])
        self.assertTrue(s["touched_rust"])
        s2 = V.summarize([{"file": "b.py", "line": 2}], [])
        self.assertFalse(s2["touched_rust"])


if __name__ == "__main__":
    unittest.main(verbosity=2)
