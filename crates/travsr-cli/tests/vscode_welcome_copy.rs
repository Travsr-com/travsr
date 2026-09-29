//! Plan 3.0 on the VS Code welcome page: the first thing a new user reads in
//! the editor uses the same plain words as `travsr init`. The page lives in
//! TypeScript, so this reads its body text and runs the shared banned-word list
//! over it (the same way `travsr-indexer/tests/typescript.rs` reads the source).

use std::path::Path;

#[test]
fn welcome_page_reads_plainly() {
    let path =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../packages/travsr-vscode/src/welcome.ts");
    let src = std::fs::read_to_string(&path).unwrap();
    let body = src
        .split("<body>")
        .nth(1)
        .and_then(|b| b.split("</body>").next())
        .expect("welcome page has a body");

    // Drop the markup, keep the words.
    let mut text = String::new();
    let mut in_tag = false;
    for c in body.chars() {
        match c {
            '<' => in_tag = true,
            '>' => {
                in_tag = false;
                text.push(' ');
            }
            _ if !in_tag => text.push(c),
            _ => {}
        }
    }

    assert_eq!(
        travsr_plugin_host::phase_b::status::jargon_in(&text),
        None,
        "{text}"
    );
    assert!(!text.contains('\u{2014}'), "em-dash in: {text}");
}
