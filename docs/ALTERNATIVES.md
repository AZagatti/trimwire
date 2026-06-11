# Alternatives & fallbacks

trimwire is the recommended path (Anthropic-documented `ANTHROPIC_BASE_URL`
gateway, no CA cert, one binary). But two other approaches exist — documented
here honestly so you can pick what fits. See [`SPIKE.md` §8](https://github.com/AZagatti/trimwire/blob/main/SPIKE.md) for
why these are "documented, not built."

## cozempic

[`Ruya-AI/cozempic`](https://github.com/Ruya-AI/cozempic) is a session-JSON
pruner: it reads Claude Code's on-disk transcript files and rewrites them
in-place using 18 pruning strategies across three tiers (with a daemon mode).
It operates on stored sessions rather than live API traffic — so it complements
rather than overlaps trimwire, which prunes in-flight requests transparently.
`trimwire sweep` addresses the same use-case (transcript pruning) with a focus
on safety guarantees (atomic write, backup, undo).

**When to choose:** if you want to prune existing on-disk transcripts without
running a gateway, or you already use cozempic's 18-strategy breadth.

## HTTPS proxy via mitmproxy

Install [mitmproxy](https://mitmproxy.org) and point it at the
[`pathakmukul/claude-code-context-pruner`](https://github.com/pathakmukul/claude-code-context-pruner)
addon, then set `HTTPS_PROXY` and `NODE_EXTRA_CA_CERTS` as described in that
project's README. This approach intercepts TLS (needs a per-process CA cert) and
splits the workflow across two tools, but it's mature and you may already run
mitmproxy. trimwire uses the documented gateway path instead and exposes savings
via `trimwire stats`.

**When to choose:** if you already run mitmproxy for other purposes and prefer to
add a single addon over installing a separate binary.

## tmux restart on gateway failure

If you run Claude Code inside `tmux` and the gateway becomes unavailable
mid-session, a short reference script is available that sends an interrupt to the
right pane and runs `claude --resume <session-id>` to reconnect. It is not a
shipped feature — it only helps inside tmux/screen, and the failure it addresses
(gateway crash mid-session) is rare and recoverable by hand in seconds
(`claude --resume <id>`).

**When to choose:** if you run Claude Code inside tmux and want an automated
recovery script for the rare case of a gateway crash mid-session.
