//! G1 — SCIP symbol name/kind extraction helpers for the unification pass.
//!
//! This module is **pure** (no I/O, no DB).  The actual DB writes live in
//! `travsr-daemon::scip_unifier` which bridges indexer output with the store.
//!
//! RFC-014 §G1. Go language first; other languages added per acceptance metrics.

/// Extract `(name, travsr_kind)` from a Go SCIP symbol string.
///
/// SCIP-go symbol format: `<scheme> <mgr> <pkg> <version> <descriptor-chain>`
/// where the last `/`-separated descriptor encodes the identifier + a suffix:
///   `().`  → function or method (kind `"function"`)
///   `#`    → type / struct / interface (kind `"class"`)
///   `.`    → field / variable (kind `"variable"`, single `.` only)
pub fn go_name_kind(symbol: &str) -> Option<(&str, &str)> {
    let descriptor_chain = symbol.split_whitespace().last()?;
    let leaf = descriptor_chain
        .rsplit('/')
        .next()
        .unwrap_or(descriptor_chain);

    if leaf.ends_with("().") {
        let name = leaf.trim_end_matches("().");
        // For methods `Type#Method().` — take the part after `#`.
        let name = name.rsplit('#').next().unwrap_or(name);
        if name.is_empty() {
            return None;
        }
        Some((name, "function"))
    } else if leaf.ends_with('#') {
        let name = leaf.trim_end_matches('#');
        if name.is_empty() {
            return None;
        }
        Some((name, "class"))
    } else if leaf.ends_with('.') {
        // Single `.` suffix — field/var. Exclude the `().` case already handled above.
        let name = leaf.trim_end_matches('.');
        if name.is_empty() || name.contains('/') {
            return None;
        }
        Some((name, "variable"))
    } else {
        None
    }
}

/// Extract the raw SCIP symbol string from a node's VName signature.
///
/// scip-reader packs signatures as `"scip:{rel_path}:{symbol}"`.
/// `ingest_scip_g2` stores just the raw symbol.  Both cases are handled.
pub fn scip_symbol_from_sig(sig: &str) -> &str {
    if let Some(rest) = sig.strip_prefix("scip:") {
        if let Some(pos) = rest.find(':') {
            return &rest[pos + 1..];
        }
    }
    sig
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn go_name_kind_function() {
        assert_eq!(
            go_name_kind("scip-go go github.com/org/repo v1.0.0 pkg/HandleRequest()."),
            Some(("HandleRequest", "function"))
        );
    }

    #[test]
    fn go_name_kind_method() {
        assert_eq!(
            go_name_kind("scip-go go github.com/org/repo v1.0.0 pkg/Server#Serve()."),
            Some(("Serve", "function"))
        );
    }

    #[test]
    fn go_name_kind_type() {
        assert_eq!(
            go_name_kind("scip-go go github.com/org/repo v1.0.0 pkg/Server#"),
            Some(("Server", "class"))
        );
    }

    #[test]
    fn go_name_kind_interface() {
        assert_eq!(
            go_name_kind("scip-go go github.com/org/repo v1.0.0 pkg/Handler#"),
            Some(("Handler", "class"))
        );
    }

    #[test]
    fn go_name_kind_variable() {
        assert_eq!(
            go_name_kind("scip-go go github.com/org/repo v1.0.0 pkg/ErrNotFound."),
            Some(("ErrNotFound", "variable"))
        );
    }

    #[test]
    fn go_name_kind_no_suffix() {
        assert_eq!(go_name_kind("scip-go go pkg v1.0.0 something"), None);
    }

    #[test]
    fn scip_symbol_from_scip_reader_sig() {
        let sig = "scip:pkg/foo.go:scip-go go example.com v1.0.0 pkg/Foo#";
        assert_eq!(
            scip_symbol_from_sig(sig),
            "scip-go go example.com v1.0.0 pkg/Foo#"
        );
    }

    #[test]
    fn scip_symbol_from_raw_sig() {
        let sig = "scip-go go example.com v1.0.0 pkg/Bar().";
        assert_eq!(scip_symbol_from_sig(sig), sig);
    }
}
