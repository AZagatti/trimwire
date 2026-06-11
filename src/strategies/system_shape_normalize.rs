//! `system_shape_normalize` (POC, opt-in, OFF by default) — repair a malformed
//! body where Claude Code emits a `role:"system"` entry as `messages[0]`.
//!
//! Anthropic's API rejects `role:"system"` inside `messages[]` with a hard 400
//! ("system" is only valid as the top-level `system` field). Claude Code has
//! been observed emitting this shape after `/compact`, `/clear`, or a model
//! switch (see `claude-code-cache-fix` issue #172), bricking the request. Since
//! trimwire sits on the wire, it can lift that stray entry's content into the
//! top-level `system` field and drop the malformed message — turning a
//! guaranteed 400 into a valid request.
//!
//! **Safe by construction:** fires ONLY on the exact malformed shape
//! (`messages[0].role == "system"`). A well-formed body is never touched. It
//! does NOT clobber an existing top-level `system` (the rare both-present case is
//! ambiguous to merge, so it's left untouched). This is the only place trimwire
//! mutates the `system`/message-structure boundary — hence opt-in, default OFF —
//! but it only ever acts on a body that would otherwise 400.

use serde_json::Value;

/// If `root.messages[0].role == "system"` and there is no top-level `system`,
/// move that entry's `content` to `root.system` and drop the entry. Returns
/// whether the body was changed. Deterministic + idempotent.
pub fn normalize(root: &mut Value) -> bool {
    // Precise guard: only the malformed `messages[0].role == "system"` shape.
    let is_stray_system = root
        .get("messages")
        .and_then(Value::as_array)
        .and_then(|m| m.first())
        .and_then(|m0| m0.get("role"))
        .and_then(Value::as_str)
        == Some("system");
    if !is_stray_system {
        return false;
    }
    // Don't clobber an existing top-level `system` — merging two system sources
    // is ambiguous (different shapes), so leave the (rare) both-present case
    // untouched rather than risk dropping or duplicating instructions.
    if root.get("system").is_some_and(|s| !s.is_null()) {
        return false;
    }

    let content = {
        let Some(messages) = root.get_mut("messages").and_then(Value::as_array_mut) else {
            return false;
        };
        // Require at least one OTHER message to remain — never empty `messages[]`
        // (an empty array is itself a 400). A stray-system-only body is already
        // broken beyond what this repair can fix, so leave it untouched.
        if messages.len() < 2 {
            return false;
        }
        // Only lift if the stray entry actually carries content — writing a
        // null/absent top-level `system` is itself a 400, so a content-less stray
        // is left untouched rather than swapped for a different broken state.
        if messages[0].get("content").is_none() {
            return false;
        }
        let stray = messages.remove(0);
        stray.get("content").cloned().unwrap_or(Value::Null)
    };
    root["system"] = content;
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn lifts_stray_system_to_top_level() {
        let mut root = json!({
            "model": "claude",
            "messages": [
                {"role": "system", "content": "you are helpful"},
                {"role": "user", "content": "hi"}
            ]
        });
        assert!(normalize(&mut root), "should normalize the malformed shape");
        assert_eq!(root["system"], json!("you are helpful"), "content lifted");
        let msgs = root["messages"].as_array().unwrap();
        assert_eq!(msgs.len(), 1, "stray system message removed");
        assert_eq!(
            msgs[0]["role"],
            json!("user"),
            "first message is now the user turn"
        );
    }

    #[test]
    fn array_system_content_is_lifted_verbatim() {
        let mut root = json!({
            "messages": [
                {"role": "system", "content": [{"type":"text","text":"sys"}]},
                {"role": "user", "content": "hi"}
            ]
        });
        assert!(normalize(&mut root));
        assert_eq!(root["system"], json!([{"type":"text","text":"sys"}]));
    }

    #[test]
    fn does_not_clobber_existing_top_level_system() {
        let mut root = json!({
            "system": "real system",
            "messages": [
                {"role": "system", "content": "stray"},
                {"role": "user", "content": "hi"}
            ]
        });
        assert!(!normalize(&mut root), "both-present case is left untouched");
        assert_eq!(
            root["system"],
            json!("real system"),
            "existing system unchanged"
        );
        assert_eq!(
            root["messages"].as_array().unwrap().len(),
            2,
            "nothing removed"
        );
    }

    #[test]
    fn well_formed_body_is_untouched() {
        let mut root = json!({
            "system": "sys",
            "messages": [
                {"role": "user", "content": "hi"},
                {"role": "assistant", "content": "yo"}
            ]
        });
        let before = root.clone();
        assert!(!normalize(&mut root), "no malformed shape → no-op");
        assert_eq!(root, before);
    }

    #[test]
    fn no_top_level_system_and_well_formed_is_untouched() {
        let mut root = json!({"messages": [{"role": "user", "content": "hi"}]});
        let before = root.clone();
        assert!(!normalize(&mut root));
        assert_eq!(root, before);
    }

    #[test]
    fn stray_system_only_is_left_untouched_never_empties_messages() {
        // If the stray system is the ONLY message, removing it would leave an
        // empty messages[] (itself a 400) — so refuse to normalize.
        let mut root = json!({"messages": [{"role": "system", "content": "sys"}]});
        let before = root.clone();
        assert!(
            !normalize(&mut root),
            "stray-system-only body left untouched"
        );
        assert_eq!(root, before, "messages[] not emptied");
    }

    #[test]
    fn content_less_stray_system_is_left_untouched() {
        // A role:"system" entry with NO content field: lifting it would write a
        // null top-level system (itself a 400), so leave the body untouched.
        let mut root = json!({
            "messages": [
                {"role": "system"},
                {"role": "user", "content": "hi"}
            ]
        });
        let before = root.clone();
        assert!(!normalize(&mut root), "content-less stray left untouched");
        assert_eq!(root, before, "no system:null written, no message removed");
    }

    #[test]
    fn idempotent() {
        let mut root = json!({
            "messages": [
                {"role": "system", "content": "sys"},
                {"role": "user", "content": "hi"}
            ]
        });
        assert!(normalize(&mut root), "first pass normalizes");
        assert!(!normalize(&mut root), "second pass is a no-op");
        assert_eq!(root["system"], json!("sys"));
        assert_eq!(root["messages"].as_array().unwrap().len(), 1);
    }
}
