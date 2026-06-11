"""Minimal request-capture proxy for Phase 0 fixture collection.

Runs an HTTP server on 127.0.0.1:8765, accepts POST /v1/messages from
Claude Code (with ANTHROPIC_BASE_URL=http://127.0.0.1:8765), redacts
the Authorization header, writes the JSON body to a file, and returns
an error response so claude exits without retrying many times.

Usage:
  python3 tests/phase0/capture.py --output tests/fixtures/<name>.json &
  ANTHROPIC_BASE_URL=http://127.0.0.1:8765 claude --print "<prompt>"
  kill %1

Then manually review the captured JSON for any personal content that
needs `[REDACTED]` substitution (paths, names, emails, etc.) before
committing.
"""

from __future__ import annotations

import argparse
import http.server
import json
import socketserver
import sys
from pathlib import Path


def make_handler(output_path: Path):
    class Handler(http.server.BaseHTTPRequestHandler):
        def log_message(self, fmt: str, *args) -> None:
            pass  # Suppress access log to keep stderr clean

        def do_POST(self) -> None:
            length = int(self.headers.get("Content-Length", 0))
            raw = self.rfile.read(length) if length else b""
            try:
                body = json.loads(raw)
            except json.JSONDecodeError:
                self.send_response(400)
                self.end_headers()
                return

            output_path.write_text(json.dumps(body, indent=2))
            print(f"[capture] wrote {len(raw)}B to {output_path}", file=sys.stderr)

            self.send_response(200)
            self.send_header("Content-Type", "application/json")
            self.end_headers()
            # Return an error body so claude exits without retrying
            self.wfile.write(b'{"type":"error","error":{"type":"test_capture","message":"captured"}}')

    return Handler


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--output", required=True, help="Path to write captured JSON body")
    ap.add_argument("--port", type=int, default=8765)
    args = ap.parse_args()

    out = Path(args.output)
    out.parent.mkdir(parents=True, exist_ok=True)

    print(f"[capture] listening on 127.0.0.1:{args.port}; will write to {out}", file=sys.stderr)
    with socketserver.TCPServer(("127.0.0.1", args.port), make_handler(out)) as httpd:
        httpd.serve_forever()
    return 0


if __name__ == "__main__":
    sys.exit(main())
