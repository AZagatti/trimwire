#!/usr/bin/env bash
# Seed a throwaway ledger so `trimwire stats` shows representative numbers when
# recording the demo GIF. Requires sqlite3. Set TRIMWIRE_LEDGER__DB_PATH first,
# then `source` this (the demo.tape does so in a Hidden block).
#
# Schema mirrors the real v4 ledger (id + v2 strategy_bytes + v3 response-metric
# columns + v4 model) and sets user_version=4, so `stats` renders every section
# (per-strategy bytes, cache health, per-model split) instead of blanks.
set -euo pipefail
db="${TRIMWIRE_LEDGER__DB_PATH:?set TRIMWIRE_LEDGER__DB_PATH before sourcing}"
mkdir -p "$(dirname "$db")"
rm -f "$db"
sqlite3 "$db" "
CREATE TABLE requests (
  id INTEGER PRIMARY KEY,
  ts INTEGER NOT NULL, session_id TEXT,
  in_bytes INTEGER NOT NULL, out_bytes INTEGER NOT NULL,
  strategies TEXT NOT NULL DEFAULT '', prefix_hash_in TEXT NOT NULL, prefix_hash_out TEXT NOT NULL,
  strategy_bytes TEXT NOT NULL DEFAULT '',
  ttft_us INTEGER NOT NULL DEFAULT 0,
  input_tokens INTEGER NOT NULL DEFAULT 0,
  cache_read_input_tokens INTEGER NOT NULL DEFAULT 0,
  cache_creation_input_tokens INTEGER NOT NULL DEFAULT 0,
  output_tokens INTEGER NOT NULL DEFAULT 0,
  applied_edits_cleared_thinking_turns INTEGER NOT NULL DEFAULT 0,
  applied_edits_cleared_tool_uses INTEGER NOT NULL DEFAULT 0,
  applied_edits_cleared_input_tokens INTEGER NOT NULL DEFAULT 0,
  model TEXT
) STRICT;
PRAGMA user_version = 4;
INSERT INTO requests
  (ts, session_id, in_bytes, out_bytes, strategies, prefix_hash_in, prefix_hash_out,
   strategy_bytes, ttft_us, input_tokens, cache_read_input_tokens,
   cache_creation_input_tokens, output_tokens, model)
VALUES
  (strftime('%s','now'),'demo',420000,168000,'cross_turn_dedup,image_strip','a','a',
   'cross_turn_dedup:180000,image_strip:72000',310000,105000,84000,21000,4200,'claude-opus-4-6'),
  (strftime('%s','now'),'demo',310000,150000,'failed_input_purge','b','b',
   'failed_input_purge:160000',280000,77000,60000,17000,3100,'claude-sonnet-4-6'),
  (strftime('%s','now'),'demo',280000,120000,'bloat_cap','c','c',
   'bloat_cap:160000',260000,70000,55000,15000,2800,'claude-opus-4-6');
"
