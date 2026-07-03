"""Unit tests for ai_review_track — classification + per-persona rollup. Stdlib only."""
import sys
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import ai_review_track as T  # noqa: E402


class TestClassify(unittest.TestCase):
    def test_thumbs_down_is_rejected(self):
        self.assertEqual(T.classify({"-1": 1}, []), "rejected")

    def test_confused_is_rejected(self):
        self.assertEqual(T.classify({"confused": 2}, []), "rejected")

    def test_reject_keyword_reply(self):
        self.assertEqual(T.classify({}, ["this is a false positive, ignoring"]), "rejected")
        self.assertEqual(T.classify({}, ["wontfix — out of scope"]), "rejected")

    def test_thumbs_up_is_accepted(self):
        self.assertEqual(T.classify({"+1": 1}, []), "accepted")

    def test_accept_keyword_reply(self):
        self.assertEqual(T.classify({}, ["good catch, fixed"]), "accepted")
        self.assertEqual(T.classify({}, ["thanks, addressed in the next commit"]), "accepted")

    def test_no_signal_is_open(self):
        self.assertEqual(T.classify({}, []), "open")
        self.assertEqual(T.classify({"eyes": 3}, ["hmm let me think"]), "open")

    def test_reject_takes_precedence(self):
        # a thumbs-up AND a "false positive" reply -> reject wins (stronger signal)
        self.assertEqual(T.classify({"+1": 1}, ["actually this is a false positive"]), "rejected")

    def test_malformed_inputs_dont_crash(self):
        self.assertEqual(T.classify(None, None), "open")
        self.assertEqual(T.classify("nope", [42, None]), "open")


class TestFinalize(unittest.TestCase):
    def test_attaches_status(self):
        recs = [{"personas": ["WARDEN"], "reactions": {"-1": 1}, "replies": []}]
        out = T.finalize(recs)
        self.assertEqual(out[0]["status"], "rejected")

    def test_preserves_existing_valid_status(self):
        recs = [{"personas": ["X"], "status": "accepted", "reactions": {"-1": 1}}]
        out = T.finalize(recs)
        self.assertEqual(out[0]["status"], "accepted")  # not reclassified

    def test_skips_non_dicts(self):
        self.assertEqual(T.finalize(["bad", None, {"reactions": {}}]), [{"reactions": {}, "status": "open"}])


class TestSummarize(unittest.TestCase):
    def test_per_persona_counts_and_rate(self):
        recs = [
            {"personas": ["WARDEN"], "status": "accepted"},
            {"personas": ["WARDEN"], "status": "rejected"},
            {"personas": ["WARDEN"], "status": "open"},
            {"personas": ["SENTINEL"], "status": "accepted"},
        ]
        per = T.summarize(recs)
        self.assertEqual(per["WARDEN"]["total"], 3)
        self.assertEqual(per["WARDEN"]["accept_rate"], 0.5)   # 1 acc / (1 acc + 1 rej); open excluded
        self.assertEqual(per["SENTINEL"]["accept_rate"], 1.0)

    def test_multi_persona_finding_counts_for_each(self):
        per = T.summarize([{"personas": ["A", "B"], "status": "accepted"}])
        self.assertEqual(per["A"]["accepted"], 1)
        self.assertEqual(per["B"]["accepted"], 1)

    def test_rate_none_when_all_open(self):
        per = T.summarize([{"personas": ["X"], "status": "open"}])
        self.assertIsNone(per["X"]["accept_rate"])

    def test_missing_personas_bucketed(self):
        per = T.summarize([{"status": "accepted"}])
        self.assertIn("(none)", per)


class TestRenderSummary(unittest.TestCase):
    def test_table_and_noisiest_first(self):
        per = {
            "GOOD": {"total": 2, "accepted": 2, "rejected": 0, "open": 0, "accept_rate": 1.0},
            "NOISY": {"total": 3, "accepted": 1, "rejected": 2, "open": 0, "accept_rate": 0.333},
        }
        md = T.render_summary_md(per)
        self.assertIn("| Persona |", md)
        self.assertLess(md.index("NOISY"), md.index("GOOD"))  # lowest accept_rate first

    def test_empty_has_placeholder(self):
        self.assertIn("no data yet", T.render_summary_md({}))


class TestParseMeta(unittest.TestCase):
    def test_parses_personas_and_consensus(self):
        meta = T.parse_meta("nice finding\n<!-- ai-review-meta personas=WARDEN,SENTINEL consensus=2 -->")
        self.assertEqual(meta["personas"], ["WARDEN", "SENTINEL"])
        self.assertEqual(meta["consensus"], 2)

    def test_no_marker_returns_none(self):
        self.assertIsNone(T.parse_meta("just a normal comment"))

    def test_missing_consensus_is_none(self):
        meta = T.parse_meta("<!-- ai-review-meta personas=X -->")
        self.assertEqual(meta["personas"], ["X"])
        self.assertIsNone(meta["consensus"])

    def test_last_marker_wins_over_spoofed_earlier_one(self):
        # a spoofed marker embedded in a suggestion block (earlier) must NOT win over the
        # real appended one (last) — attribution can't be forged from finding content
        body = ("```suggestion\nlet x = 1; <!-- ai-review-meta personas=FAKE consensus=99 -->\n```"
                "\n\n<!-- ai-review-meta personas=WARDEN consensus=1 -->")
        meta = T.parse_meta(body)
        self.assertEqual(meta["personas"], ["WARDEN"])
        self.assertEqual(meta["consensus"], 1)


class TestMine(unittest.TestCase):
    def _comments(self):
        return [
            {"id": 1, "user": {"type": "Bot"}, "in_reply_to_id": None,
             "path": "src/a.rs", "line": 10, "pull_request_url": "https://api.gh/repos/o/r/pulls/141",
             "body": "bug here <!-- ai-review-meta personas=WARDEN consensus=1 -->",
             "reactions": {"-1": 1}},
            {"id": 2, "user": {"type": "User"}, "in_reply_to_id": 1, "body": "false positive, ignore"},
            {"id": 3, "user": {"type": "Bot"}, "in_reply_to_id": None,
             "path": "src/b.rs", "line": 5, "pull_request_url": "https://api.gh/repos/o/r/pulls/141",
             "body": "no marker here"},                          # not tracked (no marker)
            {"id": 4, "user": {"type": "User"}, "in_reply_to_id": None, "body": "human comment"},
        ]

    def test_keeps_only_marked_bot_roots(self):
        recs = T.mine(self._comments())
        self.assertEqual(len(recs), 1)
        r = recs[0]
        self.assertEqual(r["personas"], ["WARDEN"])
        self.assertEqual(r["pr"], "141")
        self.assertEqual(r["file"], "src/a.rs")
        self.assertEqual(r["replies"], ["false positive, ignore"])

    def test_mined_record_classifies_rejected(self):
        recs = T.finalize(T.mine(self._comments()))
        self.assertEqual(recs[0]["status"], "rejected")   # -1 reaction + reject reply

    def test_ignores_malformed(self):
        self.assertEqual(T.mine(["x", None, {"user": {"type": "Bot"}, "body": "no marker"}]), [])


if __name__ == "__main__":
    unittest.main(verbosity=2)
