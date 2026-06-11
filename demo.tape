# demo.tape — record the trimwire demo GIF.
#   1. install VHS:  https://github.com/charmbracelet/vhs
#   2. build:        cargo build   (debug is fine — or `cargo build --release`)
#   3. record:       vhs demo.tape  → writes docs/demo.gif
#
# Safe to run: it does NOT install anything or modify your system. It seeds a
# throwaway ledger in a temp dir and runs read-only commands. Requires sqlite3.
# Run it from the repo root; it finds trimwire in target/{release,debug}.
Output docs/demo.gif
Set Shell bash
Set FontSize 18
Set Width 1200
Set Height 760
Set Padding 18
Set Theme "Catppuccin Mocha"
Set TypingSpeed 45ms

# Setup (hidden): put the locally-built binary on PATH and seed a throwaway
# ledger so `stats` shows representative numbers.
Hide
Type "export PATH=$PWD/target/release:$PWD/target/debug:$PATH" Enter
Type "export TRIMWIRE_LEDGER__ENABLED=true TRIMWIRE_LEDGER__DB_PATH=$(mktemp -d)/demo.db" Enter
Type "source scripts/demo-seed.sh && clear" Enter
Show

Type "# trimwire prunes every Claude Code request on the wire" Enter
Sleep 1s
Type "trimwire --help" Enter
Sleep 3s
Type "clear" Enter

Type "# a live gateway line — bytes shrink before they leave your machine:" Enter
Sleep 800ms
Type@30ms "echo '[gateway] POST /v1/messages in=205982B sent=69470B status=200 15ms pruned[cross_turn_dedup: 4 stubbed, 121420B; image_strip: 2 stubbed, 15092B]'" Enter
Sleep 2.5s

Type "# running totals:" Enter
Sleep 600ms
Type "trimwire stats" Enter
Sleep 4s
