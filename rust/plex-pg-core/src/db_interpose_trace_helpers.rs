use std::env;
use std::fs::File;
use std::io::{BufRead, BufReader};

pub(crate) fn list_contains_idx(list: &str, idx: i32) -> bool {
    if list.is_empty() {
        return false;
    }
    if list.eq_ignore_ascii_case("all") {
        return true;
    }
    for token in list.split(|c: char| c == ',' || c == ';' || c.is_ascii_whitespace()) {
        if token.is_empty() {
            continue;
        }
        if let Ok(v) = token.parse::<i32>() {
            if v == idx {
                return true;
            }
        }
    }
    false
}

pub(crate) fn list_any_token_in_haystack(list: &str, haystack: &str) -> bool {
    for token in list
        .split(|c: char| c == ',' || c == ';' || c == '\n')
        .map(|t| t.trim())
    {
        if !token.is_empty() && haystack.contains(token) {
            return true;
        }
    }
    false
}

pub(crate) fn trim_first_line(line: &str) -> Option<String> {
    let trimmed = line.trim_matches(|c: char| c == '\n' || c == '\r' || c == ' ' || c == '\t');
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

pub(crate) fn getenv_nonempty(key: &str) -> Option<String> {
    match env::var(key) {
        Ok(v) if !v.is_empty() => Some(v),
        _ => None,
    }
}

pub(crate) fn read_first_line_trimmed(path: &str) -> Option<String> {
    let file = File::open(path).ok()?;
    let mut reader = BufReader::new(file);
    let mut line = String::new();
    reader.read_line(&mut line).ok()?;
    trim_first_line(&line)
}
