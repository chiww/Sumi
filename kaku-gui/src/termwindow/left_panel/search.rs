//! The Search tool: find text in the project's files.
//!
//! The walk honours `.gitignore` and skips hidden and binary files, like
//! ripgrep's defaults. It runs on a worker thread; a newer query bumps the
//! generation counter and the older walk stops at its next file.

use std::collections::HashSet;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

/// Results stop here; the panel says the list is cut.
pub(crate) const MAX_MATCHES: usize = 2000;
/// Larger files are skipped: they are build output or data, not source.
const MAX_FILE_BYTES: u64 = 4 * 1024 * 1024;
/// Characters of a matching line kept for display.
const LINE_PREVIEW_CHARS: usize = 160;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct SearchOptions {
    pub query: String,
    pub match_case: bool,
    pub regex: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct LineMatch {
    /// 1-based line number.
    pub line: usize,
    /// The line, trimmed and shortened for display.
    pub text: String,
    /// Matched character ranges within `text`.
    pub ranges: Vec<(usize, usize)>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct FileMatches {
    pub path: PathBuf,
    pub matches: Vec<LineMatch>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct SearchOutcome {
    pub files: Vec<FileMatches>,
    pub total: usize,
    pub truncated: bool,
    pub error: Option<String>,
}

#[derive(Default)]
pub(crate) struct SearchState {
    pub options: SearchOptions,
    pub root: Option<PathBuf>,
    pub outcome: SearchOutcome,
    pub running: bool,
    /// Files whose matches are folded away.
    pub collapsed: HashSet<PathBuf>,
    generation: Arc<AtomicU64>,
}

impl SearchState {
    /// Starts a new generation, cancelling any walk in flight.
    pub fn next_generation(&self) -> (u64, Arc<AtomicU64>) {
        let generation = self.generation.fetch_add(1, Ordering::SeqCst) + 1;
        (generation, Arc::clone(&self.generation))
    }

    pub fn is_current(&self, generation: u64) -> bool {
        self.generation.load(Ordering::SeqCst) == generation
    }
}

pub(crate) fn build_regex(options: &SearchOptions) -> Result<regex::Regex, String> {
    let pattern = if options.regex {
        options.query.clone()
    } else {
        regex::escape(&options.query)
    };
    regex::RegexBuilder::new(&pattern)
        .case_insensitive(!options.match_case)
        .build()
        .map_err(|err| err.to_string())
}

fn looks_binary(path: &Path) -> bool {
    let Ok(mut file) = std::fs::File::open(path) else {
        return true;
    };
    let mut head = [0u8; 8192];
    let n = file.read(&mut head).unwrap_or(0);
    head[..n].contains(&0)
}

/// The display form of a matching line: leading whitespace dropped and,
/// for long lines, a window that starts a little before the first match.
pub(crate) fn preview(line: &str, regex: &regex::Regex) -> (String, Vec<(usize, usize)>) {
    let chars: Vec<char> = line.chars().collect();
    let byte_to_char = |byte: usize| line[..byte].chars().count();
    let ranges: Vec<(usize, usize)> = regex
        .find_iter(line)
        .filter(|m| m.start() != m.end())
        .map(|m| (byte_to_char(m.start()), byte_to_char(m.end())))
        .collect();
    let indent = chars.iter().take_while(|c| c.is_whitespace()).count();
    let first = ranges.first().map_or(indent, |r| r.0);
    let start = if first.saturating_sub(indent) > LINE_PREVIEW_CHARS / 3 {
        first - LINE_PREVIEW_CHARS / 6
    } else {
        indent
    };
    let end = (start + LINE_PREVIEW_CHARS).min(chars.len());
    let mut text: String = chars[start..end].iter().collect();
    if start > indent {
        text.insert(0, '…');
    }
    let shift = if start > indent { 1 } else { 0 };
    let ranges = ranges
        .into_iter()
        .filter(|(s, _)| *s >= start && *s < end)
        .map(|(s, e)| (s - start + shift, e.min(end) - start + shift))
        .collect();
    (text, ranges)
}

/// Searches every text file under `root`. Returns `None` when a newer
/// generation took over before the walk finished.
pub(crate) fn run_search(
    root: &Path,
    options: &SearchOptions,
    generation: u64,
    current: &AtomicU64,
) -> Option<SearchOutcome> {
    let regex = match build_regex(options) {
        Ok(regex) => regex,
        Err(err) => {
            return Some(SearchOutcome {
                error: Some(err),
                ..Default::default()
            })
        }
    };
    let mut outcome = SearchOutcome::default();
    let mut paths: Vec<PathBuf> = ignore::WalkBuilder::new(root)
        .hidden(true)
        .git_ignore(true)
        .require_git(false)
        .build()
        .filter_map(|entry| entry.ok())
        .filter(|entry| entry.file_type().is_some_and(|t| t.is_file()))
        .map(|entry| entry.into_path())
        .collect();
    paths.sort();
    for path in paths {
        if current.load(Ordering::SeqCst) != generation {
            return None;
        }
        if outcome.total >= MAX_MATCHES {
            outcome.truncated = true;
            break;
        }
        let too_big = std::fs::metadata(&path)
            .map(|meta| meta.len() > MAX_FILE_BYTES)
            .unwrap_or(true);
        if too_big || looks_binary(&path) {
            continue;
        }
        let Ok(content) = std::fs::read_to_string(&path) else {
            continue;
        };
        let mut matches = vec![];
        for (idx, line) in content.lines().enumerate() {
            if !regex.is_match(line) {
                continue;
            }
            let (text, ranges) = preview(line, &regex);
            matches.push(LineMatch {
                line: idx + 1,
                text,
                ranges,
            });
            outcome.total += 1;
            if outcome.total >= MAX_MATCHES {
                outcome.truncated = true;
                break;
            }
        }
        if !matches.is_empty() {
            outcome.files.push(FileMatches { path, matches });
        }
    }
    Some(outcome)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn opts(query: &str, match_case: bool, regex: bool) -> SearchOptions {
        SearchOptions {
            query: query.into(),
            match_case,
            regex,
        }
    }

    #[test]
    fn plain_queries_are_literal_and_case_follows_the_toggle() {
        let re = build_regex(&opts("a.b", false, false)).unwrap();
        assert!(re.is_match("A.B"));
        assert!(!re.is_match("axb"));
        let re = build_regex(&opts("a.b", true, true)).unwrap();
        assert!(re.is_match("axb"));
        assert!(!re.is_match("AXB"));
        assert!(build_regex(&opts("(", false, true)).is_err());
    }

    #[test]
    fn previews_trim_indent_and_keep_match_ranges_in_chars() {
        let re = build_regex(&opts("墨", false, false)).unwrap();
        let (text, ranges) = preview("    let 名 = \"墨\";", &re);
        assert_eq!(text, "let 名 = \"墨\";");
        assert_eq!(ranges, vec![(9, 10)]);
        let long = format!("{}needle", "x".repeat(300));
        let re = build_regex(&opts("needle", false, false)).unwrap();
        let (text, ranges) = preview(&long, &re);
        assert!(text.starts_with('…'));
        let (s, e) = ranges[0];
        assert_eq!(
            text.chars().skip(s).take(e - s).collect::<String>(),
            "needle"
        );
    }

    #[test]
    fn walks_respect_gitignore_and_skip_binaries() {
        let dir = std::env::temp_dir().join(format!("sumi-search-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("target")).unwrap();
        std::fs::write(dir.join(".gitignore"), "target/\n").unwrap();
        std::fs::write(dir.join("a.rs"), "fn main() {}\n// TODO one\n").unwrap();
        std::fs::write(dir.join("target/b.rs"), "// TODO hidden\n").unwrap();
        std::fs::write(dir.join("c.bin"), b"TODO\0\x01").unwrap();
        let current = AtomicU64::new(7);
        let outcome = run_search(&dir, &opts("todo", false, false), 7, &current).unwrap();
        assert_eq!(outcome.total, 1);
        assert_eq!(outcome.files[0].path, dir.join("a.rs"));
        assert_eq!(outcome.files[0].matches[0].line, 2);
        // A newer generation cancels the walk.
        assert!(run_search(&dir, &opts("todo", false, false), 6, &current).is_none());
        std::fs::remove_dir_all(dir).unwrap();
    }
}
