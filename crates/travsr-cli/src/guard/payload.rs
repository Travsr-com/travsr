//! The Claude Code `PreToolUse` wire types (#916).
//!
//! Both directions are typed rather than poked at as `serde_json::Value`, so a
//! field the host renames fails to deserialize in one place instead of silently
//! reading as `None` five call sites deep. Every field is optional on the way
//! in: the guard has to answer a payload from a newer host that added fields,
//! and from an older one that has not got them yet, and neither is an error.
//!
//! Contract reference: <https://code.claude.com/docs/en/hooks>, `PreToolUse`.
//! The decision lives under `hookSpecificOutput`, not at the top level. An
//! earlier sketch of this feature emitted a bare `{"permissionDecision": ...}`;
//! Claude Code ignores that shape, so the guard would have been inert in
//! exactly the mode that is supposed to block.

use serde::Deserialize;

/// What the host writes on the guard's stdin.
///
/// `#[serde(default)]` throughout: a missing `session_id` (a very old host, or
/// a hand-driven test) must degrade to "no session release valve", not to a
/// parse failure that fails the whole payload open when it did not need to.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct HookInput {
    /// Session identifier. Scopes the strict-mode release valve.
    pub session_id: Option<String>,
    /// The directory the agent is working in. The repo root is resolved from it.
    pub cwd: Option<String>,
    /// `"PreToolUse"` for the event this guard handles. Anything else is passed
    /// through: the hook is only ever registered for `PreToolUse`, so another
    /// value means the settings file was hand-edited and the guard has no
    /// business deciding.
    pub hook_event_name: Option<String>,
    /// `Grep`, `Glob`, `Read`, `Bash`, or anything else the matcher let through.
    pub tool_name: Option<String>,
    /// The tool's own arguments, shape-dependent.
    pub tool_input: Option<ToolInput>,
}

/// The union of the `tool_input` fields this guard reads.
///
/// One struct rather than an enum keyed on `tool_name`, because the host is
/// free to add tools whose input overlaps these names and an unknown-variant
/// error would be a parse failure over a payload we can read perfectly well.
/// Unread fields are dropped.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct ToolInput {
    /// `Bash`: the command line.
    pub command: Option<String>,
    /// `Grep`: the regular expression.
    pub pattern: Option<String>,
    /// `Glob`: the glob. (`Grep` also accepts one, to scope the search.)
    pub glob: Option<String>,
    /// `Grep` / `Glob`: the directory to search under.
    pub path: Option<String>,
    /// `Read`: the file to read.
    pub file_path: Option<String>,
    /// `Read`: a partial read starts here.
    pub offset: Option<u64>,
    /// `Read`: a partial read stops after this many lines.
    pub limit: Option<u64>,

    // The Travsr MCP tools' own arguments. The installed hook matches them too,
    // so the guard can see that the agent has been to the graph, which is what
    // releases the strict-mode valve (see `guard::session`). It never decides
    // anything about these calls; it observes and allows.
    /// `get_callers`, `find_references`: the symbol being asked about.
    pub symbol: Option<String>,
    /// `get_context`, `get_graph_json`: the query.
    pub query: Option<String>,
    /// `search_symbol`: the name.
    pub name: Option<String>,
    /// `get_dependencies`, `get_blast_radius`: the file.
    pub file: Option<String>,
}

/// What the guard writes on stdout.
///
/// **The guard never auto-approves anything.** `permissionDecision: "allow"`
/// is not "do not block", it is "approve this without asking the user", and it
/// overrides the permission rules the user configured for that tool. The guard
/// has no standing to do that: its whole claim is that it knows which reads the
/// graph can replace, which says nothing about which paths a user is willing to
/// have read. An earlier version of this spent the auto-approve on every
/// matched call in advisory mode, which silently lifted `Read` gating on
/// `~/.ssh/id_rsa`, `.env` and anything else outside the repository, since
/// those are exactly the paths `redirect_for` declines to vouch for. That is
/// the same failure as auto-approving `grep foo && rm -rf build` on the
/// strength of its first word, on a different tool.
///
/// So only one of these carries a `permissionDecision`, and it is a `deny`:
///
/// * [`Neutral`] writes nothing. The host documents that as "no decision;
///   normal permission flow applies".
/// * [`Context`] carries the redirect and no decision, so the agent is taught
///   without the user's permission settings being spent to do it. This is
///   every advisory output, and every strict output the release valve lets
///   through.
/// * [`Deny`] is the only decision the guard ever emits, only in strict mode,
///   and only for a call `redirect_for` positively answered.
///
/// `allow` and `ask` are therefore never emitted. `ask` would put a prompt in
/// front of the user on every `grep` and teach the agent nothing either way;
/// `allow` is the hole described above.
///
/// [`Neutral`]: HookOutput::Neutral
/// [`Context`]: HookOutput::Context
/// [`Deny`]: HookOutput::Deny
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HookOutput {
    /// No JSON on stdout: the host applies its normal permission flow.
    Neutral,
    /// Context for the agent, with no permission decision attached.
    ///
    /// `additionalContext` rather than `permissionDecisionReason`, because the
    /// latter only exists alongside a decision and is display-only without one.
    /// A host that does not recognise a `hookSpecificOutput` carrying no
    /// decision simply ignores it, which degrades to [`Neutral`] and is still
    /// correct.
    ///
    /// [`Neutral`]: HookOutput::Neutral
    Context { text: String },
    /// Deny, naming the Travsr call that replaces the blocked one.
    Deny { reason: String },
}

impl HookOutput {
    /// Teach without deciding: the redirect reaches the agent and the host's
    /// own permission flow is left exactly as it was.
    pub fn context(text: impl Into<String>) -> Self {
        HookOutput::Context { text: text.into() }
    }

    /// The one decision the guard emits.
    pub fn deny(reason: impl Into<String>) -> Self {
        HookOutput::Deny {
            reason: reason.into(),
        }
    }

    /// The bytes to write on stdout. Empty for [`HookOutput::Neutral`].
    pub fn render(&self) -> String {
        let specific = match self {
            HookOutput::Neutral => return String::new(),
            HookOutput::Context { text } => serde_json::json!({
                "hookEventName": "PreToolUse",
                "additionalContext": text,
            }),
            // `permissionDecisionReason` is what the host surfaces to the agent
            // on a deny, so the replacement call belongs there rather than in
            // `additionalContext`, which would say the same thing twice.
            HookOutput::Deny { reason } => serde_json::json!({
                "hookEventName": "PreToolUse",
                "permissionDecision": "deny",
                "permissionDecisionReason": reason,
            }),
        };
        // `to_string` on an object built from `json!` cannot fail.
        serde_json::json!({ "hookSpecificOutput": specific }).to_string()
    }

    /// Whether this output blocks the tool call. The property every fail-open
    /// test asserts the negation of.
    ///
    /// Test-only: the guard itself never branches on its own decision, it
    /// renders it. This exists so a test can state the fail-open invariant
    /// once, in the vocabulary of the contract, rather than re-deriving it
    /// from a string comparison at each call site.
    #[cfg(test)]
    pub fn blocks(&self) -> bool {
        matches!(self, HookOutput::Deny { .. })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_decision_renders_under_hook_specific_output() {
        let v: serde_json::Value =
            serde_json::from_str(&HookOutput::deny("call get_callers").render()).unwrap();
        assert_eq!(v["hookSpecificOutput"]["hookEventName"], "PreToolUse");
        assert_eq!(v["hookSpecificOutput"]["permissionDecision"], "deny");
        assert_eq!(
            v["hookSpecificOutput"]["permissionDecisionReason"],
            "call get_callers"
        );
        // A deny's reason already reaches the agent; duplicating it as
        // additionalContext would print it twice.
        assert!(v["hookSpecificOutput"]["additionalContext"].is_null());
    }

    #[test]
    fn context_teaches_without_deciding() {
        let v: serde_json::Value =
            serde_json::from_str(&HookOutput::context("prefer find_references").render()).unwrap();
        assert_eq!(
            v["hookSpecificOutput"]["additionalContext"], "prefer find_references",
            "the redirect has to reach the agent"
        );
        assert!(
            v["hookSpecificOutput"]["permissionDecision"].is_null(),
            "and it must not carry a decision: {v}"
        );
    }

    /// The property the whole type exists to hold. `allow` is the host's
    /// auto-approve, so emitting it would lift the user's own permission rules
    /// for that call, and the guard's claim to know which reads the graph can
    /// replace says nothing about which paths a user will have read.
    #[test]
    fn nothing_the_guard_emits_ever_auto_approves() {
        for out in [
            HookOutput::Neutral,
            HookOutput::context("a nudge"),
            HookOutput::deny("a refusal"),
        ] {
            let rendered = out.render();
            assert!(
                !rendered.contains("\"allow\"") && !rendered.contains("\"ask\""),
                "the guard must never approve on the user's behalf: {rendered}"
            );
        }
    }

    #[test]
    fn neutral_renders_nothing_at_all() {
        assert_eq!(HookOutput::Neutral.render(), "");
        assert!(!HookOutput::Neutral.blocks());
    }

    #[test]
    fn only_a_deny_blocks() {
        assert!(HookOutput::deny("x").blocks());
        assert!(!HookOutput::context("x").blocks());
        assert!(!HookOutput::Neutral.blocks());
    }

    #[test]
    fn a_payload_missing_every_optional_field_still_parses() {
        let p: HookInput = serde_json::from_str("{}").unwrap();
        assert!(p.tool_name.is_none());
        assert!(p.tool_input.is_none());
    }

    #[test]
    fn unknown_fields_are_ignored_not_rejected() {
        let p: HookInput = serde_json::from_str(
            r#"{"tool_name":"Grep","tool_input":{"pattern":"foo","some_new_field":1},
                "a_field_from_a_newer_host":true}"#,
        )
        .expect("a newer host's extra fields must not fail the parse");
        assert_eq!(p.tool_name.as_deref(), Some("Grep"));
        assert_eq!(p.tool_input.and_then(|i| i.pattern).as_deref(), Some("foo"));
    }

    #[test]
    fn the_documented_payload_shape_round_trips() {
        // Verbatim from the hook reference, trimmed to the fields we read.
        let p: HookInput = serde_json::from_str(
            r#"{
              "session_id": "abc123",
              "transcript_path": "/home/user/.claude/projects/x/transcript.jsonl",
              "cwd": "/home/user/my-project",
              "permission_mode": "default",
              "hook_event_name": "PreToolUse",
              "tool_name": "Bash",
              "tool_input": { "command": "rg needle" },
              "tool_use_id": "toolu_01ABC123"
            }"#,
        )
        .unwrap();
        assert_eq!(p.session_id.as_deref(), Some("abc123"));
        assert_eq!(p.cwd.as_deref(), Some("/home/user/my-project"));
        assert_eq!(p.hook_event_name.as_deref(), Some("PreToolUse"));
        assert_eq!(p.tool_name.as_deref(), Some("Bash"));
        assert_eq!(
            p.tool_input.and_then(|i| i.command).as_deref(),
            Some("rg needle")
        );
    }
}
