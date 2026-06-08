use std::collections::{HashMap, HashSet};
use serde::Serialize;
use crate::command::diff::state::{Annotation, AnnotationTarget};
use crate::command::diff::types::DiffPanelFocus;

/// Whether the created review is left as a draft or submitted as a comment.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ReviewEvent {
    /// Create a pending/draft review. GitHub does this when `event` is omitted.
    Draft,
    /// Submit immediately as a non-blocking comment review (`event: "COMMENT"`).
    Comment,
}

/// Serialize ReviewEvent as the `event` field: omitted for Draft, "COMMENT" otherwise.
fn serialize_event<S>(event: &ReviewEvent, s: S) -> Result<S::Ok, S::Error>
where
    S: serde::Serializer,
{
    match event {
        ReviewEvent::Draft => s.serialize_none(),
        ReviewEvent::Comment => s.serialize_some("COMMENT"),
    }
}

/// Returns true when the event must be omitted from the JSON body.
fn event_is_draft(event: &ReviewEvent) -> bool {
    matches!(event, ReviewEvent::Draft)
}

#[derive(Debug, Serialize, PartialEq)]
pub struct ReviewComment {
    pub path: String,
    pub body: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub line: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub side: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub start_line: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub start_side: Option<String>,
    /// "file" for whole-file comments; omitted for line comments.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub subject_type: Option<String>,
}

#[derive(Debug, Serialize, PartialEq)]
pub struct ReviewPayload {
    /// Top-level review body. Omitted entirely when empty: GitHub rejects a
    /// submitted (`event: COMMENT`) review that carries an empty-string body,
    /// whereas omitting it matches how the web UI submits comment-only reviews.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub body: String,
    #[serde(serialize_with = "serialize_event", skip_serializing_if = "event_is_draft")]
    pub event: ReviewEvent,
    pub comments: Vec<ReviewComment>,
}

/// Commentable line numbers for one file, split by diff side.
/// RIGHT = new-file line numbers (added + context).
/// LEFT  = old-file line numbers (removed + context).
#[derive(Debug, Default, PartialEq)]
pub struct FileHunks {
    pub right: HashSet<usize>,
    pub left: HashSet<usize>,
}

/// Map of file path -> commentable lines, derived from a unified diff.
pub type HunkMap = HashMap<String, FileHunks>;

/// Parse a `gh pr diff` unified diff into the set of lines GitHub will accept
/// review comments on, per file and per side.
pub fn parse_hunk_map(diff: &str) -> HunkMap {
    let mut map: HunkMap = HashMap::new();
    let mut current: Option<String> = None;
    let mut old_line = 0usize;
    let mut new_line = 0usize;

    for line in diff.lines() {
        if let Some(rest) = line.strip_prefix("diff --git ") {
            // "a/path b/path" -> take the b/ path (rfind handles spaces in paths)
            let path = rest.rfind(" b/").map(|i| rest[i + 3..].to_string());
            current = path;
            if let Some(ref p) = current {
                map.entry(p.clone()).or_default();
            }
            continue;
        }

        // Hunk header: @@ -oldStart,oldCount +newStart,newCount @@
        if line.starts_with("@@") {
            match parse_hunk_header(line) {
                Some((os, ns)) => {
                    old_line = os;
                    new_line = ns;
                }
                None => {
                    current = None;
                }
            }
            continue;
        }

        // Skip file-header noise lines that aren't diff content.
        if line.starts_with("--- ") || line.starts_with("+++ ") || line.starts_with("index ") {
            continue;
        }

        let Some(path) = current.as_ref() else { continue };
        let hunks = map.entry(path.clone()).or_default();

        match line.as_bytes().first() {
            Some(b'+') => {
                hunks.right.insert(new_line);
                new_line += 1;
            }
            Some(b'-') => {
                hunks.left.insert(old_line);
                old_line += 1;
            }
            Some(b' ') => {
                hunks.left.insert(old_line);
                hunks.right.insert(new_line);
                old_line += 1;
                new_line += 1;
            }
            // "\ No newline at end of file" and blank separators: ignore.
            _ => {}
        }
    }

    map
}

/// Parse "@@ -10,4 +10,5 @@ ..." into (old_start, new_start).
fn parse_hunk_header(line: &str) -> Option<(usize, usize)> {
    let inner = line.trim_start_matches('@').trim();
    let mut parts = inner.split_whitespace();
    let old_part = parts.next()?.trim_start_matches('-'); // "10,4"
    let new_part = parts.next()?.trim_start_matches('+'); // "10,5"
    let old_start = old_part.split(',').next()?.parse().ok()?;
    let new_start = new_part.split(',').next()?.parse().ok()?;
    Some((old_start, new_start))
}

/// Map a panel to the GitHub review side string.
fn panel_side(panel: DiffPanelFocus) -> &'static str {
    match panel {
        DiffPanelFocus::Old => "LEFT",
        DiffPanelFocus::New | DiffPanelFocus::None => "RIGHT",
    }
}

/// Classify each annotation into an inline comment, file-level comment, or a
/// body roll-up, producing a complete `ReviewPayload`.
///
/// - File-target annotations -> file-level comments (`subject_type: "file"`).
/// - Line-range annotations fully inside the diff -> inline comments.
/// - Line-range annotations with any endpoint outside the diff -> rolled up
///   into the review body (entire range, no clamping).
pub fn build_review(
    annotations: &[Annotation],
    hunk_map: &HunkMap,
    diff_reference: Option<&str>,
    user_body: &str,
    event: ReviewEvent,
) -> ReviewPayload {
    let mut comments = Vec::new();
    let mut rollups = Vec::new();

    for ann in annotations {
        match &ann.target {
            AnnotationTarget::File => {
                comments.push(ReviewComment {
                    path: ann.filename.clone(),
                    body: ann.content.clone(),
                    line: None,
                    side: None,
                    start_line: None,
                    start_side: None,
                    subject_type: Some("file".to_string()),
                });
            }
            AnnotationTarget::LineRange { panel, start_line, end_line } => {
                let side = panel_side(*panel);
                let in_diff = |line: usize| match hunk_map.get(&ann.filename) {
                    Some(h) if side == "RIGHT" => h.right.contains(&line),
                    Some(h) => h.left.contains(&line),
                    None => false,
                };

                if in_diff(*start_line) && in_diff(*end_line) {
                    // An annotation's range lives on a single panel, so the
                    // start and end of a multi-line comment share one side.
                    let (start_line_field, start_side_field) = if start_line == end_line {
                        (None, None)
                    } else {
                        (Some(*start_line), Some(side.to_string()))
                    };
                    comments.push(ReviewComment {
                        path: ann.filename.clone(),
                        body: ann.content.clone(),
                        line: Some(*end_line),
                        side: Some(side.to_string()),
                        start_line: start_line_field,
                        start_side: start_side_field,
                        subject_type: None,
                    });
                } else {
                    rollups.push(format_rollup(ann, side));
                }
            }
        }
    }

    let body = assemble_body(diff_reference, user_body, &rollups);
    ReviewPayload { body, event, comments }
}

/// Format one rolled-up annotation as a markdown bullet referencing its location.
fn format_rollup(ann: &Annotation, side: &str) -> String {
    let location = match &ann.target {
        AnnotationTarget::File => ann.filename.clone(),
        AnnotationTarget::LineRange { start_line, end_line, .. } => {
            if start_line == end_line {
                format!("{} line {} ({})", ann.filename, start_line, side)
            } else {
                format!("{} lines {}-{} ({})", ann.filename, start_line, end_line, side)
            }
        }
    };
    format!("- **{}**\n  {}", location, ann.content)
}

/// Combine the user body, optional diff reference header, and roll-up bullets.
fn assemble_body(diff_reference: Option<&str>, user_body: &str, rollups: &[String]) -> String {
    let mut parts: Vec<String> = Vec::new();
    if !user_body.trim().is_empty() {
        parts.push(user_body.trim().to_string());
    }
    if !rollups.is_empty() {
        let mut section = String::new();
        if let Some(reference) = diff_reference {
            section.push_str(&format!("Notes that couldn't be anchored to the diff ({}):\n\n", reference));
        } else {
            section.push_str("Notes that couldn't be anchored to the diff:\n\n");
        }
        section.push_str(&rollups.join("\n\n"));
        parts.push(section);
    }
    parts.join("\n\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::command::diff::state::{Annotation, AnnotationTarget};
    use crate::command::diff::types::DiffPanelFocus;
    use std::time::UNIX_EPOCH;

    fn ann(id: u64, filename: &str, target: AnnotationTarget, content: &str) -> Annotation {
        Annotation {
            id,
            filename: filename.to_string(),
            target,
            content: content.to_string(),
            created_at: UNIX_EPOCH,
        }
    }

    fn sample_map() -> HunkMap {
        let mut m = HunkMap::new();
        m.insert(
            "src/foo.rs".to_string(),
            FileHunks {
                right: HashSet::from([10, 11, 12, 13]),
                left: HashSet::from([10, 11, 12]),
            },
        );
        m
    }

    #[test]
    fn build_review_makes_inline_comment_for_in_diff_line() {
        let map = sample_map();
        let annotations = vec![ann(
            1,
            "src/foo.rs",
            AnnotationTarget::LineRange { panel: DiffPanelFocus::New, start_line: 11, end_line: 11 },
            "looks wrong",
        )];
        let payload = build_review(&annotations, &map, None, "", ReviewEvent::Draft);

        assert_eq!(payload.comments.len(), 1);
        let c = &payload.comments[0];
        assert_eq!(c.path, "src/foo.rs");
        assert_eq!(c.line, Some(11));
        assert_eq!(c.side, Some("RIGHT".to_string()));
        assert_eq!(c.start_line, None);
        assert_eq!(c.body, "looks wrong");
        assert_eq!(payload.body, "");
    }

    #[test]
    fn build_review_makes_multiline_left_comment() {
        let map = sample_map();
        let annotations = vec![ann(
            1,
            "src/foo.rs",
            AnnotationTarget::LineRange { panel: DiffPanelFocus::Old, start_line: 10, end_line: 12 },
            "old block",
        )];
        let payload = build_review(&annotations, &map, None, "", ReviewEvent::Draft);

        let c = &payload.comments[0];
        assert_eq!(c.side, Some("LEFT".to_string()));
        assert_eq!(c.start_line, Some(10));
        assert_eq!(c.start_side, Some("LEFT".to_string()));
        assert_eq!(c.line, Some(12));
    }

    #[test]
    fn build_review_makes_file_level_comment() {
        let map = sample_map();
        let annotations = vec![ann(1, "src/foo.rs", AnnotationTarget::File, "whole file note")];
        let payload = build_review(&annotations, &map, None, "", ReviewEvent::Draft);

        let c = &payload.comments[0];
        assert_eq!(c.subject_type, Some("file".to_string()));
        assert_eq!(c.line, None);
        assert_eq!(c.side, None);
        assert_eq!(c.body, "whole file note");
    }

    #[test]
    fn build_review_rolls_up_out_of_diff_line_into_body() {
        let map = sample_map();
        let annotations = vec![ann(
            1,
            "src/foo.rs",
            AnnotationTarget::LineRange { panel: DiffPanelFocus::New, start_line: 99, end_line: 99 },
            "note on unchanged line",
        )];
        let payload = build_review(&annotations, &map, Some("PR #42"), "", ReviewEvent::Draft);

        assert!(payload.comments.is_empty());
        assert!(payload.body.contains("note on unchanged line"));
        assert!(payload.body.contains("src/foo.rs"));
        assert!(payload.body.contains("99"));
        assert!(payload.body.contains("PR #42"));
    }

    #[test]
    fn build_review_partial_multiline_outside_diff_rolls_up_entirely() {
        let map = sample_map();
        let annotations = vec![ann(
            1,
            "src/foo.rs",
            AnnotationTarget::LineRange { panel: DiffPanelFocus::New, start_line: 13, end_line: 14 },
            "spans out of diff",
        )];
        let payload = build_review(&annotations, &map, None, "", ReviewEvent::Draft);

        assert!(payload.comments.is_empty());
        assert!(payload.body.contains("spans out of diff"));
    }

    #[test]
    fn build_review_prepends_user_body() {
        let map = sample_map();
        let annotations = vec![ann(
            1,
            "src/foo.rs",
            AnnotationTarget::LineRange { panel: DiffPanelFocus::New, start_line: 11, end_line: 11 },
            "inline",
        )];
        let payload = build_review(&annotations, &map, None, "Overall LGTM", ReviewEvent::Comment);
        assert!(payload.body.starts_with("Overall LGTM"));
        assert_eq!(payload.event, ReviewEvent::Comment);
    }

    const SAMPLE_DIFF: &str = "\
diff --git a/src/foo.rs b/src/foo.rs
index 1111111..2222222 100644
--- a/src/foo.rs
+++ b/src/foo.rs
@@ -10,4 +10,5 @@ fn foo() {
 context_a
-removed_old_11
+added_new_11
+added_new_12
 context_b
";

    #[test]
    fn parse_hunk_map_marks_added_context_on_right_and_removed_context_on_left() {
        let map = parse_hunk_map(SAMPLE_DIFF);
        let hunks = map.get("src/foo.rs").expect("file present");

        // New side (RIGHT): context_a=10, added_new_11=11, added_new_12=12, context_b=13
        assert_eq!(hunks.right, HashSet::from([10, 11, 12, 13]));
        // Old side (LEFT): context_a=10, removed_old_11=11, context_b=12
        assert_eq!(hunks.left, HashSet::from([10, 11, 12]));
    }

    #[test]
    fn parse_hunk_map_handles_multiple_files() {
        let diff = "\
diff --git a/a.rs b/a.rs
--- a/a.rs
+++ b/a.rs
@@ -1,2 +1,2 @@
-old_a
+new_a
 ctx_a
diff --git a/b.rs b/b.rs
--- a/b.rs
+++ b/b.rs
@@ -5,1 +5,2 @@
 ctx_b
+added_b
";
        let map = parse_hunk_map(diff);
        let a = map.get("a.rs").expect("a.rs present");
        assert_eq!(a.right, HashSet::from([1, 2]));
        assert_eq!(a.left, HashSet::from([1, 2]));
        let b = map.get("b.rs").expect("b.rs present");
        assert_eq!(b.right, HashSet::from([5, 6]));
        assert_eq!(b.left, HashSet::from([5]));
    }

    #[test]
    fn parse_hunk_map_handles_hunk_header_without_counts() {
        let diff = "\
diff --git a/c.rs b/c.rs
--- a/c.rs
+++ b/c.rs
@@ -5 +5 @@
-old_c
+new_c
";
        let map = parse_hunk_map(diff);
        let c = map.get("c.rs").expect("c.rs present");
        assert_eq!(c.right, HashSet::from([5]));
        assert_eq!(c.left, HashSet::from([5]));
    }

    #[test]
    fn review_payload_serializes_draft_without_event_field() {
        let payload = ReviewPayload {
            body: "top-level note".to_string(),
            event: ReviewEvent::Draft,
            comments: vec![ReviewComment {
                path: "src/foo.rs".to_string(),
                body: "inline".to_string(),
                line: Some(11),
                side: Some("RIGHT".to_string()),
                start_line: None,
                start_side: None,
                subject_type: None,
            }],
        };
        let json = serde_json::to_value(&payload).unwrap();

        // Draft => `event` key omitted entirely (GitHub creates a PENDING review).
        assert!(json.get("event").is_none());
        assert_eq!(json["body"], "top-level note");
        assert_eq!(json["comments"][0]["path"], "src/foo.rs");
        assert_eq!(json["comments"][0]["line"], 11);
        assert_eq!(json["comments"][0]["side"], "RIGHT");
        // None fields are omitted, not null.
        assert!(json["comments"][0].get("start_line").is_none());
        assert!(json["comments"][0].get("subject_type").is_none());
    }

    #[test]
    fn review_payload_serializes_comment_event() {
        let payload = ReviewPayload {
            body: String::new(),
            event: ReviewEvent::Comment,
            comments: vec![],
        };
        let json = serde_json::to_value(&payload).unwrap();
        assert_eq!(json["event"], "COMMENT");
        // An empty body must be omitted, not sent as "": GitHub 422s a
        // submitted COMMENT review that carries an empty-string body.
        assert!(json.get("body").is_none());
    }

    #[test]
    fn build_review_rolls_up_annotation_for_file_absent_from_hunk_map() {
        let map = sample_map(); // only contains "src/foo.rs"
        let annotations = vec![ann(
            1,
            "src/other.rs",
            AnnotationTarget::LineRange { panel: DiffPanelFocus::New, start_line: 1, end_line: 1 },
            "note on untracked file",
        )];
        let payload = build_review(&annotations, &map, None, "", ReviewEvent::Draft);

        // File not in the hunk map => cannot anchor => rolled up.
        assert!(payload.comments.is_empty());
        assert!(payload.body.contains("note on untracked file"));
        assert!(payload.body.contains("src/other.rs"));
    }

    #[test]
    fn build_review_user_body_precedes_rollup_section() {
        let map = sample_map();
        let annotations = vec![ann(
            1,
            "src/foo.rs",
            // line 99 is outside the diff => rolled up.
            AnnotationTarget::LineRange { panel: DiffPanelFocus::New, start_line: 99, end_line: 99 },
            "stray note",
        )];
        let payload = build_review(&annotations, &map, None, "Overall LGTM", ReviewEvent::Comment);

        assert!(payload.body.starts_with("Overall LGTM"));
        let body_pos = payload.body.find("Overall LGTM").unwrap();
        let rollup_pos = payload.body.find("stray note").expect("rollup present");
        // User body comes before the rolled-up notes section.
        assert!(body_pos < rollup_pos);
        assert!(payload.body.contains("Notes that couldn't be anchored"));
    }
}
