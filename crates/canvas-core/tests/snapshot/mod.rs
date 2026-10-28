//! A small text-snapshot facility. Snapshots live in `tests/snapshots/` as
//! `<name>.snap`; a mismatch writes `<name>.snap.new` beside it and fails
//! with a line diff. `UPDATE_SNAPSHOTS=1` (re)writes the snapshots instead.
//! Line endings and trailing spaces are not significant.

use std::path::PathBuf;

/// Lines of context around a change in a diff.
const CONTEXT: usize = 3;
/// A diff longer than this is cut (the `.snap.new` file has everything).
const MAX_DIFF_LINES: usize = 160;

pub fn dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests").join("snapshots")
}

fn updating() -> bool {
    std::env::var("UPDATE_SNAPSHOTS").is_ok_and(|v| !v.is_empty() && v != "0")
}

/// Text as stored: `\n` line ends, no trailing spaces, one final newline.
pub fn normalize(text: &str) -> String {
    let lines: Vec<&str> = text.lines().map(str::trim_end).collect();
    let mut out = lines.join("\n");
    out.truncate(out.trim_end_matches('\n').len());
    out.push('\n');
    out
}

/// Compares `actual` with snapshot `name` (or writes it when updating).
pub fn check(name: &str, actual: &str) -> Result<(), String> {
    let actual = normalize(actual);
    let path = dir().join(format!("{name}.snap"));
    let pending = dir().join(format!("{name}.snap.new"));
    if updating() {
        std::fs::create_dir_all(dir()).map_err(|e| e.to_string())?;
        std::fs::write(&path, &actual).map_err(|e| format!("cannot write {}: {e}", path.display()))?;
        let _ = std::fs::remove_file(&pending);
        return Ok(());
    }
    let Ok(expected) = std::fs::read_to_string(&path) else {
        let _ = std::fs::write(&pending, &actual);
        return Err(format!("no snapshot {} yet: review {} and accept it with UPDATE_SNAPSHOTS=1", path.display(), pending.display()));
    };
    let expected = normalize(&expected);
    if expected == actual {
        let _ = std::fs::remove_file(&pending);
        return Ok(());
    }
    let _ = std::fs::write(&pending, &actual);
    Err(format!(
        "snapshot `{name}` differs (- snapshot, + actual). The new text is in {}; UPDATE_SNAPSHOTS=1 accepts it.\n{}",
        pending.display(),
        diff(&expected, &actual)
    ))
}

#[track_caller]
pub fn assert_snapshot(name: &str, actual: &str) {
    if let Err(message) = check(name, actual) {
        panic!("{message}");
    }
}

/// One line of a diff: kept, removed (from `expected`) or added (in `actual`).
#[derive(Clone, Copy, PartialEq, Debug)]
enum Op {
    Keep(usize, usize),
    Remove(usize),
    Add(usize),
}

/// A line diff (longest common subsequence) with line numbers and a few
/// lines of context around each change.
pub fn diff(expected: &str, actual: &str) -> String {
    let a: Vec<&str> = expected.lines().collect();
    let b: Vec<&str> = actual.lines().collect();
    let prefix = a.iter().zip(&b).take_while(|(x, y)| x == y).count();
    let suffix = a[prefix..].iter().rev().zip(b[prefix..].iter().rev()).take_while(|(x, y)| x == y).count();
    let (am, bm) = (&a[prefix..a.len() - suffix], &b[prefix..b.len() - suffix]);
    let mut ops: Vec<Op> = (0..prefix).map(|i| Op::Keep(i, i)).collect();
    if am.len().saturating_mul(bm.len()) > 4_000_000 {
        // Too large for the table: everything in between changed.
        ops.extend((0..am.len()).map(|i| Op::Remove(prefix + i)));
        ops.extend((0..bm.len()).map(|j| Op::Add(prefix + j)));
    } else {
        let (n, m) = (am.len(), bm.len());
        let mut lcs = vec![vec![0u32; m + 1]; n + 1];
        for i in (0..n).rev() {
            for j in (0..m).rev() {
                lcs[i][j] = if am[i] == bm[j] { lcs[i + 1][j + 1] + 1 } else { lcs[i + 1][j].max(lcs[i][j + 1]) };
            }
        }
        let (mut i, mut j) = (0, 0);
        while i < n || j < m {
            if i < n && j < m && am[i] == bm[j] {
                ops.push(Op::Keep(prefix + i, prefix + j));
                i += 1;
                j += 1;
            } else if i < n && (j == m || lcs[i + 1][j] >= lcs[i][j + 1]) {
                ops.push(Op::Remove(prefix + i));
                i += 1;
            } else {
                ops.push(Op::Add(prefix + j));
                j += 1;
            }
        }
    }
    ops.extend((0..suffix).map(|k| Op::Keep(a.len() - suffix + k, b.len() - suffix + k)));

    // Lines near a change are shown.
    let changed: Vec<usize> = ops.iter().enumerate().filter(|(_, op)| !matches!(op, Op::Keep(..))).map(|(k, _)| k).collect();
    let shown = |k: usize| changed.iter().any(|&c| k + CONTEXT >= c && k <= c + CONTEXT);
    let mut out = Vec::new();
    let mut gap = false;
    for (k, op) in ops.iter().enumerate() {
        if !shown(k) {
            gap = true;
            continue;
        }
        if gap && !out.is_empty() {
            out.push("   ⋮".to_owned());
        }
        gap = false;
        out.push(match *op {
            Op::Keep(i, j) => format!("  {:>4} {:>4} │ {}", i + 1, j + 1, a[i]),
            Op::Remove(i) => format!("- {:>4}      │ {}", i + 1, a[i]),
            Op::Add(j) => format!("+      {:>4} │ {}", j + 1, b[j]),
        });
    }
    let removed = ops.iter().filter(|op| matches!(op, Op::Remove(_))).count();
    let added = ops.iter().filter(|op| matches!(op, Op::Add(_))).count();
    if out.len() > MAX_DIFF_LINES {
        let more = out.len() - MAX_DIFF_LINES;
        out.truncate(MAX_DIFF_LINES);
        out.push(format!("   … {more} more diff lines"));
    }
    format!("{removed} line(s) removed, {added} added\n{}", out.join("\n"))
}
