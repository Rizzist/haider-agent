//! Shared on-screen row ordering for grouped list screens.
//!
//! A grouped screen must draw rows and move its keyboard cursor through ONE
//! sequence; deriving the drawn order separately from the cursor order is
//! exactly the 973 `/accounts` bug (↑/↓ visited raw snapshot indices while the
//! screen drew provider groups). Screens therefore store their rows already
//! grouped with [`group_by_first_seen`], render them in stored order, and move
//! the cursor over the same indices.
//!
//! The rule mirrors the sim (`tui.js:3593`, Set insertion order): groups
//! appear in the order their key is first seen, and rows keep their input
//! order within a group. `/usage` (`UsageState::display_order`, lane
//! 973-usage-ui) follows the same rule and can reuse this helper.

/// Stably regroups `items` so equal keys are contiguous: groups in
/// first-seen key order, items in input order within each group.
#[must_use]
pub fn group_by_first_seen<T, K, F>(items: Vec<T>, key: F) -> Vec<T>
where
    K: PartialEq,
    F: Fn(&T) -> K,
{
    let mut groups: Vec<(K, Vec<T>)> = Vec::new();
    for item in items {
        let item_key = key(&item);
        match groups.iter_mut().find(|(group, _)| *group == item_key) {
            Some((_, members)) => members.push(item),
            None => groups.push((item_key, vec![item])),
        }
    }
    groups
        .into_iter()
        .flat_map(|(_, members)| members)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::group_by_first_seen;

    #[test]
    fn groups_in_first_seen_order_and_keeps_order_within_groups() {
        let rows = vec![("a", 1), ("b", 2), ("a", 3), ("c", 4), ("b", 5)];
        assert_eq!(
            group_by_first_seen(rows, |row| row.0),
            vec![("a", 1), ("a", 3), ("b", 2), ("b", 5), ("c", 4)]
        );
    }

    #[test]
    fn already_grouped_input_is_unchanged() {
        let rows = vec![("a", 1), ("a", 2), ("b", 3)];
        assert_eq!(group_by_first_seen(rows.clone(), |row| row.0), rows);
    }
}
