# demo.tape — record the trimwire demo GIF.
#   1. install VHS:  https://github.com/charmbracelet/vhs
#   2. build:        cargo build   (debug is fine — or `cargo build --release`)
#   3. record:       vhs demo.tape  → writes docs/demo.gif
#
# Safe to run: `scripts/demo-setup.sh` (sourced in the Hidden block below) uses a
# throwaway temp $HOME, seeds a demo ledger, and starts ephemeral localhost-only
# helpers so `doctor` renders a full green report offline. Nothing real is
# installed or modified. Requires sqlite3 + python3 + curl. Run from the repo root.
Output docs/demo.gif
Set Shell bash
Set FontSize 18
Set Width 1200
Set Height 800
Set Padding 18
Set Theme "Catppuccin Mocha"
Set TypingSpeed 42ms

Hide
Type "source scripts/demo-setup.sh && clear" Enter
Show

Type "# trimwire trims every Claude Code request on the wire — losslessly, in colour" Enter
Sleep 1200ms
Type@28ms "echo '[gateway] POST /v1/messages in=205982B sent=69470B status=200 15ms pruned[cross_turn_dedup: 4 stubbed, 121420B; image_strip: 2 stubbed, 15092B]'" Enter
Sleep 2600ms

Type "# how much lighter your context got:" Enter
Sleep 700ms
Type "trimwire stats" Enter
Sleep 4200ms
Type "clear" Enter

Type "# one command to check everything's wired up:" Enter
Sleep 700ms
Type "trimwire doctor" Enter
Sleep 4300ms
