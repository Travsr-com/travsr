//! A small comment-aware editor for JSONC config files (`opencode.jsonc`,
//! `kilo.jsonc`), used by `travsr connect`.
//!
//! Why not parse and re-serialise: OpenCode and Kilo configs are routinely
//! hand-written with `//` comments and trailing commas. Round-tripping through
//! `serde_json` would drop every comment, which is the destructive edit the rest
//! of `connect` refuses to make. So the file is edited as text. A same-length
//! "mask" (comments and trailing commas blanked to spaces) is parsed to learn
//! the structure, every offset found in the mask is valid in the original, and
//! only one member (`<top_key>.travsr`) is ever spliced in, replaced or cut out.
//!
//! Every edit is verified before it is returned: the result must parse and must
//! equal the original document with exactly that one member changed. If it does
//! not, the edit is dropped and the caller leaves the file untouched.
//!
//! Pure text in, text out. The caller owns all I/O.

use serde_json::{json, Value};

/// The server name travsr registers under.
const SERVER: &str = "travsr";

/// Result of [`upsert`].
#[derive(Debug, PartialEq)]
pub enum Upsert {
    /// The new file content.
    Changed(String),
    /// The exact entry is already present.
    Unchanged,
    /// A new entry was needed but the caller asked not to introduce one.
    Refused,
    /// The file cannot be edited safely; the reason is user-facing.
    Skipped(&'static str),
}

/// Result of [`remove`].
#[derive(Debug, PartialEq)]
pub enum Removal {
    /// The new file content, with the entry cut out.
    Changed(String),
    /// The file held nothing but our entry (and no comments): delete it.
    DeleteFile,
    /// No travsr entry present.
    Absent,
    /// The file cannot be edited safely; the reason is user-facing.
    Skipped(&'static str),
}

const NOT_JSONC: &str = "existing file is not valid JSON or JSONC (left untouched)";
const NOT_OBJECT: &str = "top level is not a JSON object";
const UNSAFE_EDIT: &str = "could not edit safely (left untouched)";

/// Upsert `root[top_key]["travsr"] = entry`, keeping every comment and every
/// other byte of `text`. An empty `text` yields a fresh pretty-printed file
/// (with `$schema` when given).
///
/// `refuse_new` declines to introduce the entry; see `merge_json_server`.
pub fn upsert(
    text: &str,
    top_key: &str,
    entry: &Value,
    schema: Option<&str>,
    refuse_new: bool,
) -> Upsert {
    if text.trim().is_empty() {
        if refuse_new {
            return Upsert::Refused;
        }
        let mut root = serde_json::Map::new();
        if let Some(s) = schema {
            root.insert("$schema".into(), json!(s));
        }
        root.insert(top_key.into(), json!({ SERVER: entry }));
        return match serde_json::to_string_pretty(&Value::Object(root)) {
            Ok(s) => Upsert::Changed(s + "\n"),
            Err(_) => Upsert::Skipped(UNSAFE_EDIT),
        };
    }

    let Some(masked) = mask(text) else {
        return Upsert::Skipped(NOT_JSONC);
    };
    let Ok(root) = serde_json::from_str::<Value>(&masked) else {
        return Upsert::Skipped(NOT_JSONC);
    };
    let Some(root_obj) = root.as_object() else {
        return Upsert::Skipped(NOT_OBJECT);
    };
    let b = masked.as_bytes();
    let root_open = skip_ws(b, 0);
    let Some(root_members) = members(b, root_open) else {
        return Upsert::Skipped(UNSAFE_EDIT);
    };

    let entry_src = match serde_json::to_string(entry) {
        Ok(s) => s,
        Err(_) => return Upsert::Skipped(UNSAFE_EDIT),
    };

    let mut expected = root.clone();
    let new_text = match find_unique(&root_members, top_key) {
        Err(()) => return Upsert::Skipped(UNSAFE_EDIT),
        Ok(Some(mcp)) => {
            if !root_obj.get(top_key).is_some_and(Value::is_object) {
                return Upsert::Skipped("`mcp` is not a JSON object");
            }
            let Some(mcp_members) = members(b, mcp.value_start) else {
                return Upsert::Skipped(UNSAFE_EDIT);
            };
            match find_unique(&mcp_members, SERVER) {
                Err(()) => return Upsert::Skipped(UNSAFE_EDIT),
                Ok(Some(existing)) => {
                    let current: Option<Value> =
                        serde_json::from_str(&masked[existing.value_start..existing.value_end])
                            .ok();
                    if current.as_ref() == Some(entry) {
                        return Upsert::Unchanged;
                    }
                    if refuse_new {
                        return Upsert::Refused;
                    }
                    format!(
                        "{}{}{}",
                        &text[..existing.value_start],
                        entry_src,
                        &text[existing.value_end..]
                    )
                }
                Ok(None) => {
                    if refuse_new {
                        return Upsert::Refused;
                    }
                    insert_member(
                        text,
                        &masked,
                        mcp.value_start,
                        &mcp_members,
                        &format!("\"{SERVER}\": {entry_src}"),
                    )
                }
            }
        }
        Ok(None) => {
            if refuse_new {
                return Upsert::Refused;
            }
            insert_member(
                text,
                &masked,
                root_open,
                &root_members,
                &format!("\"{top_key}\": {{\"{SERVER}\": {entry_src}}}"),
            )
        }
    };

    if let Some(obj) = expected.as_object_mut() {
        let slot = obj
            .entry(top_key.to_string())
            .or_insert_with(|| json!({}))
            .as_object_mut();
        match slot {
            Some(m) => {
                m.insert(SERVER.to_string(), entry.clone());
            }
            None => return Upsert::Skipped(UNSAFE_EDIT),
        }
    }
    if parses_to(&new_text, &expected) {
        Upsert::Changed(new_text)
    } else {
        Upsert::Skipped(UNSAFE_EDIT)
    }
}

/// Cut `root[top_key]["travsr"]` out of `text`, leaving comments and every
/// other member alone. The file is deleted only when it carries no comments and
/// nothing but our entry (plus `schema`, which `upsert` writes into a file it
/// creates).
pub fn remove(text: &str, top_key: &str, schema: Option<&str>) -> Removal {
    let Some(masked) = mask(text) else {
        return Removal::Skipped(NOT_JSONC);
    };
    let Ok(root) = serde_json::from_str::<Value>(&masked) else {
        return Removal::Skipped(NOT_JSONC);
    };
    let has_entry = root
        .get(top_key)
        .and_then(Value::as_object)
        .is_some_and(|m| m.contains_key(SERVER));
    if !has_entry {
        return Removal::Absent;
    }
    let b = masked.as_bytes();
    let Some(root_members) = members(b, skip_ws(b, 0)) else {
        return Removal::Skipped(UNSAFE_EDIT);
    };
    let Ok(Some(mcp)) = find_unique(&root_members, top_key) else {
        return Removal::Skipped(UNSAFE_EDIT);
    };
    let Some(mcp_members) = members(b, mcp.value_start) else {
        return Removal::Skipped(UNSAFE_EDIT);
    };
    let Ok(Some(target)) = find_unique(&mcp_members, SERVER) else {
        return Removal::Skipped(UNSAFE_EDIT);
    };
    let idx = mcp_members
        .iter()
        .position(|m| m.key_start == target.key_start)
        .unwrap_or(0);

    let ob = text.as_bytes();
    // Cut ranges, applied back to front so earlier offsets stay valid.
    let mut cuts: Vec<(usize, usize)> = Vec::new();
    let after = skip_ws(b, target.value_end);
    if b.get(after) == Some(&b',') {
        // Take the comma and the blanks after it; the indentation before the
        // key stays and becomes the next member's.
        let mut end = after + 1;
        while ob.get(end).is_some_and(u8::is_ascii_whitespace) {
            end += 1;
        }
        cuts.push((target.key_start, end));
    } else {
        // Last member: the comma that preceded it has to go instead.
        let mut start = target.key_start;
        while start > 0 && ob[start - 1].is_ascii_whitespace() {
            start -= 1;
        }
        cuts.push((start, target.value_end));
        if idx > 0 {
            let prev = &mcp_members[idx - 1];
            let comma = skip_ws(b, prev.value_end);
            if b.get(comma) != Some(&b',') {
                return Removal::Skipped(UNSAFE_EDIT);
            }
            cuts.push((comma, comma + 1));
        }
    }
    cuts.sort_unstable_by_key(|c| std::cmp::Reverse(c.0));
    let mut new_text = text.to_string();
    for (s, e) in cuts {
        new_text.replace_range(s..e, "");
    }

    let mut expected = root.clone();
    if let Some(m) = expected.get_mut(top_key).and_then(Value::as_object_mut) {
        m.remove(SERVER);
    }
    if !parses_to(&new_text, &expected) {
        return Removal::Skipped(UNSAFE_EDIT);
    }

    let comment_free = strip_comments(text).as_deref() == Some(text);
    let mut only_ours = vec![json!({ top_key: {} })];
    if let Some(s) = schema {
        only_ours.push(json!({ "$schema": s, top_key: {} }));
    }
    if comment_free && only_ours.contains(&expected) {
        return Removal::DeleteFile;
    }
    Removal::Changed(new_text)
}

/// One `"key": value` member of an object, as byte offsets into the text.
struct Member {
    key: String,
    key_start: usize,
    value_start: usize,
    value_end: usize,
}

/// The member named `key`. `Err` when it appears twice: which one a consumer
/// honours is its own business, so we do not guess.
fn find_unique<'a>(members: &'a [Member], key: &str) -> Result<Option<&'a Member>, ()> {
    let mut hits = members.iter().filter(|m| m.key == key);
    let first = hits.next();
    if hits.next().is_some() {
        return Err(());
    }
    Ok(first)
}

/// Splice `member_src` in as the first member of the object opening at `open`,
/// copying the first existing member's indentation so pretty files stay pretty.
/// First rather than last: no trailing-comma or comment-at-the-end cases.
fn insert_member(
    text: &str,
    masked: &str,
    open: usize,
    members: &[Member],
    member_src: &str,
) -> String {
    let (lead, comma) = match members.first() {
        Some(first) => {
            let gap = &masked[open + 1..first.key_start];
            let lead = match gap.rfind('\n') {
                Some(p) => format!("\n{}", &gap[p + 1..]),
                None => gap.to_string(),
            };
            (lead, ",")
        }
        None => (String::new(), ""),
    };
    format!(
        "{}{lead}{member_src}{comma}{}",
        &text[..=open],
        &text[open + 1..]
    )
}

/// Whether `text` (comments allowed) parses to exactly `expected`.
fn parses_to(text: &str, expected: &Value) -> bool {
    mask(text)
        .and_then(|m| serde_json::from_str::<Value>(&m).ok())
        .is_some_and(|v| &v == expected)
}

/// `text` with comments and trailing commas blanked to spaces (newlines kept),
/// so it is strict JSON and every offset matches `text`. `None` for an
/// unterminated block comment.
pub fn mask(text: &str) -> Option<String> {
    let stripped = strip_comments(text)?;
    let mut out = stripped.into_bytes();
    let mut i = 0;
    while i < out.len() {
        match out[i] {
            b'"' => i = skip_string_lenient(&out, i),
            b',' => {
                let j = skip_ws(&out, i + 1);
                if matches!(out.get(j), Some(b'}' | b']')) {
                    out[i] = b' ';
                }
                i += 1;
            }
            _ => i += 1,
        }
    }
    String::from_utf8(out).ok()
}

/// Blank `//` and `/* */` comments to spaces, outside of strings.
fn strip_comments(text: &str) -> Option<String> {
    let b = text.as_bytes();
    let mut out = b.to_vec();
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'"' => i = skip_string_lenient(b, i),
            b'/' if b.get(i + 1) == Some(&b'/') => {
                while i < b.len() && b[i] != b'\n' {
                    if b[i] != b'\r' {
                        out[i] = b' ';
                    }
                    i += 1;
                }
            }
            b'/' if b.get(i + 1) == Some(&b'*') => {
                let mut j = i + 2;
                loop {
                    if j + 1 >= b.len() {
                        return None;
                    }
                    if b[j] == b'*' && b[j + 1] == b'/' {
                        j += 2;
                        break;
                    }
                    j += 1;
                }
                for c in &mut out[i..j] {
                    if *c != b'\n' && *c != b'\r' {
                        *c = b' ';
                    }
                }
                i = j;
            }
            _ => i += 1,
        }
    }
    // Only whole comments (ASCII delimiters on both ends) were overwritten with
    // ASCII, so this stays valid UTF-8.
    String::from_utf8(out).ok()
}

/// Index just past the string starting at `b[i] == '"'`; end of input if it never
/// closes (the later JSON parse rejects that).
fn skip_string_lenient(b: &[u8], i: usize) -> usize {
    skip_string(b, i).unwrap_or(b.len())
}

fn skip_string(b: &[u8], i: usize) -> Option<usize> {
    let mut j = i + 1;
    loop {
        match b.get(j)? {
            b'\\' => j += 2,
            b'"' => return Some(j + 1),
            _ => j += 1,
        }
    }
}

fn skip_ws(b: &[u8], mut i: usize) -> usize {
    while b.get(i).is_some_and(u8::is_ascii_whitespace) {
        i += 1;
    }
    i
}

/// End offset (exclusive) of the JSON value starting at `b[i]`.
fn skip_value(b: &[u8], i: usize) -> Option<usize> {
    match b.get(i)? {
        b'"' => skip_string(b, i),
        b'{' | b'[' => {
            let mut depth = 0usize;
            let mut j = i;
            loop {
                match b.get(j)? {
                    b'"' => {
                        j = skip_string(b, j)?;
                        continue;
                    }
                    b'{' | b'[' => depth += 1,
                    b'}' | b']' => {
                        depth = depth.checked_sub(1)?;
                        if depth == 0 {
                            return Some(j + 1);
                        }
                    }
                    _ => {}
                }
                j += 1;
            }
        }
        _ => {
            let mut j = i;
            while b
                .get(j)
                .is_some_and(|c| !matches!(c, b',' | b'}' | b']') && !c.is_ascii_whitespace())
            {
                j += 1;
            }
            (j > i).then_some(j)
        }
    }
}

/// The members of the object whose `{` is at `b[open]`. The text must already be
/// valid (masked) JSON; anything unexpected yields `None` rather than a guess.
fn members(b: &[u8], open: usize) -> Option<Vec<Member>> {
    if b.get(open) != Some(&b'{') {
        return None;
    }
    let mut out = Vec::new();
    let mut i = open + 1;
    loop {
        i = skip_ws(b, i);
        match b.get(i)? {
            b'}' => return Some(out),
            b',' => {
                i += 1;
                continue;
            }
            b'"' => {}
            _ => return None,
        }
        let key_start = i;
        let key_end = skip_string(b, i)?;
        let key: String = serde_json::from_slice(&b[key_start..key_end]).ok()?;
        i = skip_ws(b, key_end);
        if b.get(i) != Some(&b':') {
            return None;
        }
        let value_start = skip_ws(b, i + 1);
        let value_end = skip_value(b, value_start)?;
        out.push(Member {
            key,
            key_start,
            value_start,
            value_end,
        });
        i = value_end;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry() -> Value {
        json!({ "type": "local", "command": ["/x/travsr", "mcp", "--stdio"], "enabled": true })
    }

    fn changed(r: Upsert) -> String {
        match r {
            Upsert::Changed(s) => s,
            other => panic!("expected Changed, got {other:?}"),
        }
    }

    fn parse(text: &str) -> Value {
        serde_json::from_str(&mask(text).unwrap()).unwrap()
    }

    #[test]
    fn mask_keeps_length_and_ignores_comment_markers_in_strings() {
        let text = "{\n // c \"q\n \"u\": \"http://x/*y*/\", /* b\n */ \"a\": [1,],\n}";
        let m = mask(text).unwrap();
        assert_eq!(m.len(), text.len());
        let v: Value = serde_json::from_str(&m).unwrap();
        assert_eq!(v["u"], "http://x/*y*/");
        assert_eq!(v["a"], json!([1]));
    }

    #[test]
    fn mask_rejects_an_unterminated_block_comment() {
        assert!(mask("{ /* never closed }").is_none());
    }

    #[test]
    fn empty_input_creates_a_pretty_file_with_schema() {
        let out = changed(upsert("", "mcp", &entry(), Some("https://s"), false));
        assert!(out.starts_with("{\n"));
        let v = parse(&out);
        assert_eq!(v["$schema"], "https://s");
        assert_eq!(v["mcp"]["travsr"], entry());
    }

    #[test]
    fn upsert_keeps_comments_and_other_members() {
        let text = "{\n  // my theme\n  \"theme\": \"dark\", // keep\n  \"mcp\": {\n    // other\n    \"x\": {\"command\": [\"x\"],},\n  },\n}\n";
        let out = changed(upsert(text, "mcp", &entry(), None, false));
        assert!(out.contains("// my theme") && out.contains("// keep") && out.contains("// other"));
        let v = parse(&out);
        assert_eq!(v["mcp"]["travsr"], entry());
        assert_eq!(v["theme"], "dark");
        assert!(v["mcp"]["x"].is_object());
    }

    #[test]
    fn upsert_adds_mcp_when_missing_and_handles_empty_object() {
        let out = changed(upsert("{}", "mcp", &entry(), None, false));
        assert_eq!(parse(&out)["mcp"]["travsr"], entry());
        let out = changed(upsert(
            "// top\n{\n  \"a\": 1\n}\n",
            "mcp",
            &entry(),
            None,
            false,
        ));
        assert!(out.starts_with("// top\n"));
        assert_eq!(parse(&out)["a"], 1);
        assert_eq!(parse(&out)["mcp"]["travsr"], entry());
    }

    #[test]
    fn upsert_replaces_a_stale_entry_in_place() {
        let text = "{\"mcp\": {\"travsr\": {\"command\": [\"old\"]}, \"y\": 1}}";
        let out = changed(upsert(text, "mcp", &entry(), None, false));
        let v = parse(&out);
        assert_eq!(v["mcp"]["travsr"], entry());
        assert_eq!(v["mcp"]["y"], 1);
    }

    #[test]
    fn upsert_is_idempotent() {
        let first = changed(upsert("{\"a\": 1}", "mcp", &entry(), None, false));
        assert_eq!(
            upsert(&first, "mcp", &entry(), None, false),
            Upsert::Unchanged
        );
    }

    #[test]
    fn upsert_refuse_new_declines_but_allows_an_exact_match() {
        assert_eq!(upsert("{}", "mcp", &entry(), None, true), Upsert::Refused);
        assert_eq!(upsert("", "mcp", &entry(), None, true), Upsert::Refused);
        let present = changed(upsert("{}", "mcp", &entry(), None, false));
        assert_eq!(
            upsert(&present, "mcp", &entry(), None, true),
            Upsert::Unchanged
        );
    }

    #[test]
    fn upsert_skips_what_it_cannot_understand() {
        assert!(matches!(
            upsert("{ nope", "mcp", &entry(), None, false),
            Upsert::Skipped(_)
        ));
        assert!(matches!(
            upsert("[1]", "mcp", &entry(), None, false),
            Upsert::Skipped(_)
        ));
        assert!(matches!(
            upsert("{\"mcp\": []}", "mcp", &entry(), None, false),
            Upsert::Skipped(_)
        ));
        assert!(matches!(
            upsert("{\"mcp\": {}, \"mcp\": {}}", "mcp", &entry(), None, false),
            Upsert::Skipped(_)
        ));
    }

    #[test]
    fn remove_cuts_only_our_member_in_every_position() {
        let e = entry();
        for text in [
            // first, middle, last, only; with comments around.
            format!("{{\"mcp\": {{\"travsr\": {e}, \"b\": 1}}}}"),
            format!("{{\"mcp\": {{\"a\": 0, \"travsr\": {e}, \"b\": 1}}}}"),
            format!("{{\"mcp\": {{\"a\": 0, \"travsr\": {e}}}}}"),
            format!("{{\"mcp\": {{\"a\": 0, \"travsr\": {e},}}}}"),
            format!(
                "{{\n  \"mcp\": {{\n    // c\n    \"a\": 0,\n    \"travsr\": {e} // t\n  }}\n}}\n"
            ),
        ] {
            let Removal::Changed(out) = remove(&text, "mcp", None) else {
                panic!("expected Changed for {text}");
            };
            let v = parse(&out);
            assert!(v["mcp"].get("travsr").is_none(), "{out}");
            assert_eq!(
                v["mcp"].as_object().unwrap().len(),
                parse(&text)["mcp"].as_object().unwrap().len() - 1
            );
        }
    }

    #[test]
    fn remove_keeps_unrelated_comments() {
        let text = "{\n  // keep me\n  \"a\": 1,\n  \"mcp\": {\n    \"travsr\": {\"command\": [\"t\"]}, // gone with it\n    \"b\": 2\n  }\n}\n";
        let Removal::Changed(out) = remove(text, "mcp", None) else {
            panic!("expected Changed");
        };
        assert!(out.contains("// keep me"));
        assert_eq!(parse(&out)["mcp"], json!({ "b": 2 }));
    }

    #[test]
    fn remove_deletes_a_file_that_is_only_ours() {
        let created = changed(upsert("", "mcp", &entry(), Some("https://s"), false));
        assert_eq!(
            remove(&created, "mcp", Some("https://s")),
            Removal::DeleteFile
        );
        // A comment means the user touched it: keep the file.
        let with_comment = format!("// mine\n{created}");
        assert!(matches!(
            remove(&with_comment, "mcp", Some("https://s")),
            Removal::Changed(_)
        ));
    }

    #[test]
    fn remove_reports_absent_and_skips_bad_input() {
        assert_eq!(remove("{\"mcp\": {}}", "mcp", None), Removal::Absent);
        assert_eq!(remove("{}", "mcp", None), Removal::Absent);
        assert!(matches!(remove("{ nope", "mcp", None), Removal::Skipped(_)));
    }

    #[test]
    fn upsert_then_remove_restores_the_original_document() {
        let text = "{\n  // c\n  \"a\": 1,\n  \"mcp\": {\n    \"b\": 2\n  }\n}\n";
        let with = changed(upsert(text, "mcp", &entry(), None, false));
        let Removal::Changed(back) = remove(&with, "mcp", None) else {
            panic!("expected Changed");
        };
        assert_eq!(parse(&back), parse(text));
        assert!(back.contains("// c"));
    }

    /// Tiny deterministic generator (no proptest dev-dependency in this crate).
    struct Lcg(u64);
    impl Lcg {
        fn next(&mut self, n: usize) -> usize {
            self.0 = self
                .0
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            ((self.0 >> 33) as usize) % n
        }
    }

    /// Generated JSONC documents (comments in odd places, trailing commas, CRLF,
    /// strings holding comment markers, optional existing `mcp`): upsert must
    /// change exactly one member, keep every comment, be idempotent, and remove
    /// must restore the original value.
    #[test]
    fn generated_documents_round_trip_without_losing_anything() {
        let mut g = Lcg(0x5eed);
        for case in 0..400 {
            let nl = if g.next(4) == 0 { "\r\n" } else { "\n" };
            let ind = ["", " ", "  ", "\t"][g.next(4)];
            let mut comments: Vec<String> = Vec::new();
            let mut cm = |g: &mut Lcg, tag: &str| -> String {
                match g.next(4) {
                    0 => {
                        let c = format!("// c{case}{tag}");
                        comments.push(c.clone());
                        format!("{c}{nl}")
                    }
                    1 => {
                        let c = format!("/* b{case}{tag} */");
                        comments.push(c.clone());
                        format!("{c} ")
                    }
                    _ => String::new(),
                }
            };
            let tc = |g: &mut Lcg| if g.next(2) == 0 { "," } else { "" };

            let mut text = cm(&mut g, "top");
            text += &format!("{{{nl}");
            let mut first = true;
            let mut members: Vec<String> = Vec::new();
            for i in 0..g.next(4) {
                let c = cm(&mut g, &format!("k{i}"));
                let v = match g.next(4) {
                    0 => "\"http://h/x//y/*z*/\"".to_string(),
                    1 => "[1, 2,]".to_string(),
                    2 => "{\"n\": {\"d\": null,},}".to_string(),
                    _ => "\"a \\\" // b\"".to_string(),
                };
                members.push(format!("{ind}{c}\"k{i}\": {v}"));
                first = false;
            }
            let has_mcp = g.next(2) == 0;
            if has_mcp {
                let c = cm(&mut g, "mcp");
                let inner = if g.next(2) == 0 {
                    format!(
                        "{{{nl}{ind}{ind}\"other\": {{\"command\": [\"o\"],}}{}{nl}{ind}}}",
                        tc(&mut g)
                    )
                } else {
                    "{}".to_string()
                };
                members.push(format!("{ind}{c}\"mcp\": {inner}"));
                first = false;
            }
            let _ = first;
            let trailing = tc(&mut g);
            text += &members.join(&format!(",{nl}"));
            text += trailing;
            text += &format!("{nl}{}}}{nl}", cm(&mut g, "end"));

            let before = parse(&text);
            let out = match upsert(&text, "mcp", &entry(), None, false) {
                Upsert::Changed(o) => o,
                other => panic!("case {case}: {other:?} for\n{text}"),
            };
            for c in &comments {
                assert!(out.contains(c), "case {case} lost {c}\n{out}");
            }
            let mut expect = before.clone();
            expect
                .as_object_mut()
                .unwrap()
                .entry("mcp")
                .or_insert_with(|| json!({}))
                .as_object_mut()
                .unwrap()
                .insert("travsr".into(), entry());
            assert_eq!(parse(&out), expect, "case {case}\n{out}");
            assert_eq!(
                upsert(&out, "mcp", &entry(), None, false),
                Upsert::Unchanged,
                "case {case}"
            );
            match remove(&out, "mcp", None) {
                Removal::Changed(back) => {
                    for c in &comments {
                        assert!(back.contains(c), "case {case} lost {c} on remove\n{back}");
                    }
                    let mut want = before.clone();
                    if !has_mcp {
                        want.as_object_mut()
                            .unwrap()
                            .insert("mcp".into(), json!({}));
                    }
                    assert_eq!(parse(&back), want, "case {case}\n{back}");
                }
                Removal::DeleteFile => {
                    assert!(
                        comments.is_empty()
                            && (before == json!({}) || before == json!({"mcp": {}})),
                        "case {case}"
                    )
                }
                other => panic!("case {case}: {other:?}"),
            }
        }
    }

    #[test]
    fn remove_never_deletes_when_other_top_level_keys_exist() {
        let text = format!("{{\"a\": 1, \"mcp\": {{\"travsr\": {}}}}}", entry());
        assert!(matches!(remove(&text, "mcp", None), Removal::Changed(_)));
        let text = format!("{{\"mcp\": {{\"travsr\": {}}}}}", entry());
        assert_eq!(remove(&text, "mcp", None), Removal::DeleteFile);
        // Another server alongside ours: keep the file.
        let text = format!("{{\"mcp\": {{\"travsr\": {}, \"o\": {{}}}}}}", entry());
        assert!(matches!(remove(&text, "mcp", None), Removal::Changed(_)));
    }

    #[test]
    fn bom_and_non_ascii_content_never_corrupt_the_file() {
        let bom = "\u{feff}{\"a\": 1}";
        match upsert(bom, "mcp", &entry(), None, false) {
            Upsert::Skipped(_) => {}
            Upsert::Changed(o) => assert!(parse(&o)["mcp"]["travsr"].is_object()),
            other => panic!("{other:?}"),
        }
        let uni = "{\"name\": \"caf\u{e9} \u{1f600} // x\", // note \u{e9}\n \"mcp\": {}}";
        let out = changed(upsert(uni, "mcp", &entry(), None, false));
        assert!(out.contains("caf\u{e9} \u{1f600} // x") && out.contains("// note \u{e9}"));
        assert_eq!(parse(&out)["mcp"]["travsr"], entry());
    }
}
