use std::collections::{HashMap, HashSet};
use serde::Serialize;

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

#[cfg(test)]
mod tests {
    use super::*;

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
    }
}
