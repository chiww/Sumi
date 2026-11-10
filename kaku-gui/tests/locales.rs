//! Gate the rust-i18n bundle in `locales/` against the code that reads it.
//!
//! The Simplified Chinese UI was reverted once (b4d779a) because a key the
//! code asked for was missing from en.yml, so English menus showed raw keys
//! such as `commands.menu.settings`. This test fails on exactly that class:
//!
//! 1. A YAML file rust-i18n cannot load, including a duplicate key (the
//!    later one would silently win and can wipe a whole namespace).
//! 2. A literal `t!("a.b.c")` key in Rust source that en.yml or zh-CN.yml
//!    does not define.
//! 3. A key in en.yml that zh-CN.yml lacks, or a zh-CN key outside the
//!    slug-lookup namespaces that en.yml lacks.
//! 4. A `%{placeholder}` set that differs between the two files.
//!
//! Slug-lookup namespaces (`commands.brief|doc|menubar`, `config_tui`) are
//! optional per key: the code derives the key from the English label and
//! falls back to that label, so a missing translation shows English.

use regex::Regex;
use serde_yaml::Value;
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

const SOURCE_DIRS: &[&str] = &["kaku/src", "kaku-gui/src", "config/src"];
const SLUG_NAMESPACES: &[&str] = &[
    "commands.brief.",
    "commands.doc.",
    "commands.menubar.",
    "config_tui.",
];

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("kaku-gui lives in the workspace root")
        .to_path_buf()
}

fn flatten(value: &Value, prefix: &str, out: &mut BTreeMap<String, String>) {
    match value {
        Value::Mapping(map) => {
            for (key, child) in map {
                let key = key
                    .as_str()
                    .unwrap_or_else(|| panic!("{}: non-string key {:?}", prefix, key));
                flatten(child, &format!("{prefix}{key}."), out);
            }
        }
        Value::String(text) => {
            out.insert(prefix.trim_end_matches('.').to_string(), text.clone());
        }
        Value::Null => {}
        other => panic!("{}: expected a string, found {:?}", prefix, other),
    }
}

fn load(name: &str) -> BTreeMap<String, String> {
    let path = workspace_root().join("locales").join(name);
    let text = std::fs::read_to_string(&path).expect("read locale file");
    let value: Value = serde_yaml::from_str(&text)
        .unwrap_or_else(|err| panic!("locales/{} does not parse: {}", name, err));
    let mut out = BTreeMap::new();
    flatten(&value, "", &mut out);
    out
}

fn literal_keys() -> BTreeMap<String, String> {
    let pattern = Regex::new(r#"(?:^|[^A-Za-z0-9_])t!\(\s*"([a-z0-9_]+(?:\.[a-z0-9_]+)+)""#)
        .expect("valid regex");
    let root = workspace_root();
    let mut found = BTreeMap::new();
    for dir in SOURCE_DIRS {
        for entry in walkdir::WalkDir::new(root.join(dir)) {
            let entry = entry.expect("walk source tree");
            if entry.path().extension().is_none_or(|ext| ext != "rs") {
                continue;
            }
            let text = std::fs::read_to_string(entry.path()).expect("read source");
            for caps in pattern.captures_iter(&text) {
                let at = caps.get(1).expect("key group");
                let line = text[..at.start()].matches('\n').count() + 1;
                let shown = entry.path().strip_prefix(&root).unwrap_or(entry.path());
                found
                    .entry(at.as_str().to_string())
                    .or_insert_with(|| format!("{}:{line}", shown.display()));
            }
        }
    }
    found
}

fn placeholders(text: &str) -> BTreeSet<String> {
    let pattern = Regex::new(r"%\{([A-Za-z0-9_]+)\}").expect("valid regex");
    pattern
        .captures_iter(text)
        .map(|caps| caps[1].to_string())
        .collect()
}

#[test]
fn locale_bundles_cover_every_key_the_code_reads() {
    let en = load("en.yml");
    let zh = load("zh-CN.yml");
    let mut problems = Vec::new();

    let literals = literal_keys();
    assert!(
        !literals.is_empty(),
        "found no t!() keys; the source scan is broken"
    );
    for (key, at) in &literals {
        for (name, table) in [("en.yml", &en), ("zh-CN.yml", &zh)] {
            if !table.contains_key(key) {
                problems.push(format!(
                    "{at}: t!(\"{key}\") is missing from locales/{name}"
                ));
            }
        }
    }

    for key in en.keys() {
        if !zh.contains_key(key) {
            problems.push(format!("locales/zh-CN.yml: missing {key}"));
        }
    }
    for key in zh.keys() {
        let slug = SLUG_NAMESPACES.iter().any(|ns| key.starts_with(ns));
        if !slug && !en.contains_key(key) {
            problems.push(format!("locales/en.yml: missing {key}"));
        }
    }

    for (key, english) in &en {
        if let Some(chinese) = zh.get(key) {
            if placeholders(english) != placeholders(chinese) {
                problems.push(format!("locales: placeholders differ for {key}"));
            }
        }
    }

    assert!(problems.is_empty(), "\n{}", problems.join("\n"));
}

#[test]
fn duplicate_locale_keys_are_rejected() {
    // serde_yaml refuses duplicate mapping keys, which is what makes the
    // load() above a duplicate-key gate; keep that assumption pinned.
    let err = serde_yaml::from_str::<Value>("init:\n  a: x\ninit: {}\n");
    assert!(err.is_err(), "duplicate keys must not load");
}
