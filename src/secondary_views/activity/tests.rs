use super::*;
use crate::activity::KindTag;
use crate::models::SearchMessageLocation;
use crate::secondary_views::model::ActivityTarget;

pub(crate) fn row(ts: &str, snippet: &str) -> ActivityRow {
    ActivityRow {
        key: ActivityKey {
            channel_id: "C1".to_string(),
            ts: ts.to_string(),
            kind: KindTag::Mention,
        },
        author_id: Some("U1".to_string()),
        author: "Alice".to_string(),
        others: 0,
        time: "5m".to_string(),
        snippet: snippet.to_string(),
        context: "#general".to_string(),
        private: false,
        reaction: None,
        unread: 1,
        target: ActivityTarget::Message(
            SearchMessageLocation::new("C1", ts, None).expect("location"),
        ),
    }
}

fn rc(rows: &[ActivityRow]) -> Vec<Rc<ActivityRow>> {
    rows.iter().cloned().map(Rc::new).collect()
}

#[test]
fn perf_guard_activity_unchanged_rows_are_not_rebound() {
    let rows = vec![row("3.0", "c"), row("2.0", "b"), row("1.0", "a")];
    let kept = kept_pairs(&rc(&rows), &rows, &|_| true);
    assert_eq!(kept, [(0, 0), (1, 1), (2, 2)]);
    assert!(splices_around(&kept, 3, 3).is_empty());
}

#[test]
fn perf_guard_activity_a_new_item_splices_only_itself() {
    let old = vec![row("2.0", "b"), row("1.0", "a")];
    let new = vec![row("3.0", "new"), row("2.0", "b"), row("1.0", "a")];
    let kept = kept_pairs(&rc(&old), &new, &|_| true);
    assert_eq!(kept, [(0, 1), (1, 2)]);
    assert_eq!(splices_around(&kept, 2, 3), [(0, 0, 0..1)]);
}

#[test]
fn a_changed_row_or_avatar_is_replaced_and_neighbours_are_kept() {
    let old = vec![row("3.0", "c"), row("2.0", "b"), row("1.0", "a")];
    let mut new = old.clone();
    new[1].unread = 0;
    let kept = kept_pairs(&rc(&old), &new, &|_| true);
    assert_eq!(kept, [(0, 0), (2, 2)]);
    assert_eq!(splices_around(&kept, 3, 3), [(1, 1, 1..2)]);

    let kept = kept_pairs(&rc(&old), &old, &|row| row.key.ts != "1.0");
    assert_eq!(kept, [(0, 0), (1, 1)]);
}

#[test]
fn reordered_rows_keep_the_longest_ascending_run() {
    let old = vec![row("3.0", "c"), row("2.0", "b")];
    let new = vec![row("2.0", "b"), row("3.0", "c")];
    let kept = kept_pairs(&rc(&old), &new, &|_| true);
    assert_eq!(kept.len(), 1);
}
