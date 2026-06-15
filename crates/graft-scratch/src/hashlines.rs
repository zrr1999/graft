use graft_core::HashlineEdit;

use crate::{Result, ScratchError};

pub(crate) const HASHLINE_ALPHABET: &[u8; 16] = b"ZPMQVRWSNKTXJBYH";

pub(crate) fn render_hashlines(text: &str) -> String {
    logical_lines(text)
        .iter()
        .enumerate()
        .map(|(idx, line)| format!("{}#{}:{}\n", idx + 1, line_hash(idx + 1, line), line))
        .collect()
}

pub(crate) fn line_hash(line_number: usize, line: &str) -> String {
    let mut hasher = blake3::Hasher::new();
    if !line.chars().any(|ch| ch.is_alphanumeric()) {
        hasher.update(line_number.to_string().as_bytes());
        hasher.update(b"\0");
    }
    hasher.update(line.as_bytes());
    let digest = hasher.finalize();
    let bytes = digest.as_bytes();
    let first = HASHLINE_ALPHABET[(bytes[0] & 0x0f) as usize] as char;
    let second = HASHLINE_ALPHABET[(bytes[1] & 0x0f) as usize] as char;
    format!("{first}{second}")
}

pub(crate) fn logical_lines(text: &str) -> Vec<String> {
    let text = text.strip_suffix('\n').unwrap_or(text);
    if text.is_empty() {
        return Vec::new();
    }
    text.split('\n')
        .map(|line| line.strip_suffix('\r').unwrap_or(line).to_string())
        .collect()
}

pub(crate) fn reject_display_prefixes(edits: &[HashlineEdit]) -> Result<()> {
    fn check_line(line: &str) -> Result<()> {
        if looks_like_display_prefixed_line(line) {
            return Err(ScratchError::InvalidPatch(
                "replacement line contains LINE#HASH display prefix".to_string(),
            ));
        }
        Ok(())
    }

    for edit in edits {
        match edit {
            HashlineEdit::ReplaceLine { new, .. } => check_line(new)?,
            HashlineEdit::ReplaceRange { new_lines, .. }
            | HashlineEdit::InsertAfter { new_lines, .. }
            | HashlineEdit::InsertBefore { new_lines, .. } => {
                for line in new_lines {
                    check_line(line)?;
                }
            }
            HashlineEdit::ReplaceText { new_text, .. } => {
                for line in new_text.lines() {
                    check_line(line)?;
                }
            }
        }
    }
    Ok(())
}

fn looks_like_display_prefixed_line(line: &str) -> bool {
    let Some((line_number, rest)) = line.split_once('#') else {
        return false;
    };
    !line_number.is_empty()
        && line_number.chars().all(|ch| ch.is_ascii_digit())
        && rest.len() >= 3
        && rest.as_bytes().get(2) == Some(&b':')
        && rest[..2]
            .chars()
            .all(|ch| (HASHLINE_ALPHABET.as_slice()).contains(&(ch as u8)))
}

pub(crate) fn apply_edits(text: &str, edits: &[HashlineEdit]) -> Result<String> {
    let mut lines = logical_lines(text);
    for edit in edits {
        match edit {
            HashlineEdit::ReplaceLine {
                line,
                hash,
                old,
                new,
            } => {
                let idx = checked_line_index(*line, lines.len())?;
                verify_anchor(*line, hash, old, &lines[idx], &lines)?;
                lines[idx] = new.clone();
            }
            HashlineEdit::ReplaceRange {
                start_line,
                start_hash,
                end_line,
                end_hash,
                new_lines,
            } => {
                if start_line > end_line {
                    return Err(ScratchError::InvalidPatch(
                        "replace_range start_line is after end_line".to_string(),
                    ));
                }
                let start_idx = checked_line_index(*start_line, lines.len())?;
                let end_idx = checked_line_index(*end_line, lines.len())?;
                let start_actual_hash = line_hash(*start_line as usize, &lines[start_idx]);
                let end_actual_hash = line_hash(*end_line as usize, &lines[end_idx]);
                let start_stale = &start_actual_hash != start_hash;
                let end_stale = &end_actual_hash != end_hash;
                if start_stale || end_stale {
                    let stale_line = if start_stale { *start_line } else { *end_line };
                    let stale_expected = if start_stale {
                        start_hash.clone()
                    } else {
                        end_hash.clone()
                    };
                    let stale_actual = if start_stale {
                        start_actual_hash
                    } else {
                        end_actual_hash
                    };
                    // Always include both endpoints in the fresh-anchor
                    // block so a single-end stale does not erase the
                    // surviving end’s anchor; this is what callers need to
                    // re-anchor a range edit without re-reading the file.
                    let fresh_anchors =
                        render_range_context(&lines, *start_line as usize, *end_line as usize);
                    return Err(ScratchError::StaleAnchor {
                        line: stale_line,
                        expected_hash: stale_expected,
                        actual_hash: stale_actual,
                        fresh_anchors,
                    });
                }
                lines.splice(start_idx..=end_idx, new_lines.clone());
            }
            HashlineEdit::InsertAfter {
                line,
                hash,
                new_lines,
            } => {
                let idx = checked_line_index(*line, lines.len())?;
                let old = lines[idx].clone();
                verify_anchor(*line, hash, &old, &old, &lines)?;
                lines.splice(idx + 1..idx + 1, new_lines.clone());
            }
            HashlineEdit::InsertBefore {
                line,
                hash,
                new_lines,
            } => {
                let idx = checked_line_index(*line, lines.len())?;
                let old = lines[idx].clone();
                verify_anchor(*line, hash, &old, &old, &lines)?;
                lines.splice(idx..idx, new_lines.clone());
            }
            HashlineEdit::ReplaceText { old_text, new_text } => {
                let matches = text_matches(&lines.join("\n"), old_text);
                if matches != 1 {
                    return Err(ScratchError::AmbiguousText { matches });
                }
                lines = logical_lines(&lines.join("\n").replace(old_text, new_text));
            }
        }
    }
    Ok(if lines.is_empty() {
        String::new()
    } else {
        format!("{}\n", lines.join("\n"))
    })
}

fn checked_line_index(line: u64, len: usize) -> Result<usize> {
    let idx = line
        .checked_sub(1)
        .ok_or(ScratchError::LineOutOfRange(line))? as usize;
    if idx >= len {
        return Err(ScratchError::LineOutOfRange(line));
    }
    Ok(idx)
}

fn verify_anchor(
    line: u64,
    expected_hash: &str,
    expected_text: &str,
    actual_text: &str,
    lines: &[String],
) -> Result<()> {
    let actual_hash = line_hash(line as usize, actual_text);
    if actual_hash != expected_hash || actual_text != expected_text {
        return Err(ScratchError::StaleAnchor {
            line,
            expected_hash: expected_hash.to_string(),
            actual_hash,
            fresh_anchors: render_context(lines, line as usize),
        });
    }
    Ok(())
}

fn render_context(lines: &[String], target_line: usize) -> String {
    let start = target_line.saturating_sub(2).max(1);
    let end = (target_line + 1).min(lines.len());
    (start..=end)
        .map(|line_number| {
            let text = &lines[line_number - 1];
            let marker = if line_number == target_line {
                ">>> "
            } else {
                ""
            };
            format!(
                "{marker}{line_number}#{}:{text}\n",
                line_hash(line_number, text)
            )
        })
        .collect()
}

/// Render fresh anchors for a `replace_range` edit, marking both endpoints
/// with `>>>` so callers can re-anchor without losing the surviving end when
/// only one end has drifted. Returns a single block, not two, when start and
/// end are close enough that their context windows overlap.
fn render_range_context(lines: &[String], start_line: usize, end_line: usize) -> String {
    if lines.is_empty() {
        return String::new();
    }
    let start_window_lo = start_line.saturating_sub(2).max(1);
    let start_window_hi = (start_line + 1).min(lines.len());
    let end_window_lo = end_line.saturating_sub(2).max(1);
    let end_window_hi = (end_line + 1).min(lines.len());
    if start_window_hi + 1 >= end_window_lo {
        // Windows overlap: merge into one contiguous block, marking both
        // start and end with `>>>`.
        let lo = start_window_lo.min(end_window_lo);
        let hi = start_window_hi.max(end_window_hi);
        return (lo..=hi)
            .map(|n| render_anchor_line(lines, n, n == start_line || n == end_line))
            .collect();
    }
    let mut out = String::new();
    for n in start_window_lo..=start_window_hi {
        out.push_str(&render_anchor_line(lines, n, n == start_line));
    }
    out.push_str("...\n");
    for n in end_window_lo..=end_window_hi {
        out.push_str(&render_anchor_line(lines, n, n == end_line));
    }
    out
}

fn render_anchor_line(lines: &[String], line_number: usize, is_target: bool) -> String {
    let text = &lines[line_number - 1];
    let marker = if is_target { ">>> " } else { "" };
    format!(
        "{marker}{line_number}#{}:{text}\n",
        line_hash(line_number, text)
    )
}

fn text_matches(haystack: &str, needle: &str) -> usize {
    if needle.is_empty() {
        return 0;
    }
    haystack.match_indices(needle).count()
}
