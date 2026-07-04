#!/usr/bin/env bash
# demo-setup.sh — prepare a throwaway, OFFLINE environment for recording the
# trimwire demo GIF (`demo.tape` sources this in a Hidden block).
#
# Safe to run: it uses a temp $HOME, seeds a throwaway ledger, and starts a few
# ephemeral localhost-only helpers so `trimwire doctor` renders a full, green,
# network-free report — a local gateway (so /healthz is up) plus two tiny stub
# servers that stand in for ollama's /api/tags and GitHub's releases API. It does
# NOT install anything, touch your real config, or make outbound network calls.
# The background helpers are orphaned when the recording shell exits; on a normal
# machine they're harmless and go away on reboot (or `pkill -f trimwire-demo`).
#
# Requires: sqlite3, python3, curl. Run from the repo root: `source` it.
set -uo pipefail

repo="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
export PATH="$repo/target/release:$repo/target/debug:$PATH"

# Throwaway HOME + config so nothing real is touched.
export HOME="$(mktemp -d)"
mkdir -p "$HOME/.config"
export XDG_CONFIG_HOME="$HOME/.config"
cat > "$HOME/.config/trimwire.toml" <<CFG
[server]
listen = "127.0.0.1:59947"
upstream = "https://api.anthropic.com"
[summarizer]
engine = "local"
[summarizer.local]
endpoint = "http://127.0.0.1:51447"
model = "qwen3.5:4b"
[reprune]
enabled = true
[ledger]
enabled = true
db_path = "$HOME/demo.db"
CFG

# Seed a representative ledger so `trimwire stats` shows real numbers.
export TRIMWIRE_LEDGER__DB_PATH="$HOME/demo.db"
source "$repo/scripts/demo-seed.sh"

# Tiny offline HTTP/1.1 stub: any GET returns $BODY. Used for the ollama /api/tags
# and GitHub-releases endpoints so `doctor` is green and never hits the network.
_demo_stub() { # $1=port  $2=body
  TRIMWIRE_DEMO_BODY="$2" python3 - "$1" <<'PY' &
import http.server, socketserver, os, sys
socketserver.TCPServer.allow_reuse_address = True
BODY = os.environ["TRIMWIRE_DEMO_BODY"].encode()
class H(http.server.BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"  # doctor's raw probe requires HTTP/1.1
    def do_GET(self):
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(BODY)))
        self.send_header("Connection", "close")
        self.end_headers()
        self.wfile.write(BODY)
    def log_message(self, *a):
        pass
socketserver.TCPServer(("127.0.0.1", int(sys.argv[1])), H).serve_forever()
PY
}

_demo_stub 51447 '{"models":[{"name":"qwen3.5:4b"}]}'
ver="$(grep -m1 '^version' "$repo/Cargo.toml" | sed -E 's/.*"([^"]+)".*/\1/')"
_demo_stub 51997 "{\"tag_name\":\"v$ver\",\"assets\":[]}"
export TRIMWIRE_UPDATE_API_BASE="http://127.0.0.1:51997"

# A real local gateway so `doctor` shows a serving, wired-up, green report.
trimwire serve >/dev/null 2>&1 &
export ANTHROPIC_BASE_URL="http://127.0.0.1:59947"

# Wait until the gateway and the ollama stub answer (best-effort, ~5s cap each).
for _ in $(seq 1 25); do curl -sf http://127.0.0.1:59947/healthz  >/dev/null 2>&1 && break; sleep 0.2; done
for _ in $(seq 1 20); do curl -sf http://127.0.0.1:51447/api/tags >/dev/null 2>&1 && break; sleep 0.2; done
set +e
