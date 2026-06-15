//! MUGEN trigger-group contiguity rule (`triggerall` / `trigger1..n` semantics).
//!
//! A MUGEN state controller fires when **every** `triggerall` condition is true
//! (logical AND across them) **and at least one** numbered trigger group is
//! fully true (logical OR across the groups; within a group the same-number
//! lines are AND-ed). This module owns the one subtle part of that rule — which
//! numbered groups are actually *active* — so every consumer (the state
//! executor, validators, tooling) applies identical semantics.
//!
//! ## The contiguity rule
//!
//! Numbered groups are `trigger1`, `trigger2`, … MUGEN walks them in ascending
//! order starting at `1` and **stops at the first gap** in the numbering. Any
//! group at or after a gap is *dead* — it never contributes to whether the
//! controller fires, even if its condition would be true:
//!
//! | Groups present      | Active (this module) | Dead       |
//! |---------------------|----------------------|------------|
//! | `1, 2, 3`           | `1, 2, 3`            | —          |
//! | `1, 2, 4`           | `1, 2`               | `4`        |
//! | `2, 3` (no `1`)     | — (cannot fire)      | `2, 3`     |
//! | `1, 3, 4`           | `1`                  | `3, 4`     |
//!
//! A controller with no `trigger1` can never fire: the active prefix is empty,
//! so the OR-over-groups has nothing true to find.
//!
//! ## Why this lives in `fp-vm`
//!
//! `fp-formats`'s CNS parser intentionally preserves **all** numbered groups it
//! sees (including post-gap ones) and defers the contiguity rule to the trigger
//! *consumer* — see [`fp_formats::cns::StateController::triggers`]. `fp-vm` is
//! that consumer's expression engine and the lowest crate every trigger
//! evaluator shares, so the rule is implemented here once and reused, rather
//! than re-derived in each downstream crate (this resolves backlog item CB6).
//!
//! The helpers are deliberately keyed on the group *numbers* only, decoupling
//! the rule from any particular compiled-trigger representation: a caller maps
//! its own group type to `u32` numbers, asks which are active, then evaluates
//! just those.

/// Returns the indices (into `numbers`) of the numbered trigger groups that are
/// **active** under the MUGEN contiguity rule: the contiguous run `1, 2, 3, …`
/// starting at `trigger1`, stopping at the first gap.
///
/// `numbers[i]` is the `N` of the `i`-th group's `triggerN`. Input order is
/// irrelevant — groups are considered in ascending number, and the returned
/// indices point back into the caller's original slice so the caller can map
/// straight to its own (unsorted) group data. Duplicate numbers (rare: the CNS
/// parser AND-folds same-number lines into one group) collapse to the first
/// index seen for that number and do not break contiguity.
///
/// # Examples
///
/// ```
/// use fp_vm::triggers::active_group_indices;
///
/// // Contiguous: all kept (in ascending-number order).
/// assert_eq!(active_group_indices(&[1, 2, 3]), vec![0, 1, 2]);
///
/// // Gap after 2: trigger4 (index 2) is dead.
/// assert_eq!(active_group_indices(&[1, 2, 4]), vec![0, 1]);
///
/// // Input order does not matter; indices point back into the input.
/// assert_eq!(active_group_indices(&[2, 1, 4]), vec![1, 0]);
///
/// // No trigger1: nothing is active, so the controller cannot fire.
/// assert!(active_group_indices(&[2, 3]).is_empty());
/// ```
pub fn active_group_indices(numbers: &[u32]) -> Vec<usize> {
    // Sort indices by their group number so file order does not matter, while
    // still returning indices into the caller's original slice.
    let mut by_number: Vec<usize> = (0..numbers.len()).collect();
    by_number.sort_by_key(|&i| numbers[i]);

    let mut active: Vec<usize> = Vec::new();
    let mut expected: u32 = 1;
    for idx in by_number {
        let n = numbers[idx];
        if n < expected {
            // A duplicate of an already-consumed number (`expected - 1`). The
            // CNS parser folds same-number lines into a single group, so this
            // is rare; skip it defensively without breaking the contiguous run.
            continue;
        }
        if n == expected {
            active.push(idx);
            expected += 1;
        } else {
            // First gap reached — everything from here on is dead (CB6).
            break;
        }
    }
    active
}

/// Returns `true` if any **active** numbered group is satisfied, i.e. whether
/// the OR-over-groups half of the trigger rule passes.
///
/// `numbers[i]` is the `triggerN` number of the `i`-th group and
/// `group_is_true(i)` reports whether group `i`'s conditions are all true (the
/// caller does the actual expression evaluation). Only groups in the active
/// contiguous prefix are consulted, and they are visited in ascending-number
/// order; evaluation short-circuits on the first true group, so a caller's
/// `group_is_true` for a dead or later group may never run.
///
/// This does **not** account for `triggerall`; the caller must separately
/// require every `triggerall` condition before relying on this result.
///
/// # Examples
///
/// ```
/// use fp_vm::triggers::any_active_group_true;
///
/// // group 1 false, group 2 true -> fires (OR across the contiguous prefix).
/// let truth = [false, true];
/// assert!(any_active_group_true(&[1, 2], |i| truth[i]));
///
/// // The only true group sits past a gap (trigger3, no trigger2) -> dead.
/// let truth = [false, true];
/// assert!(!any_active_group_true(&[1, 3], |i| truth[i]));
/// ```
pub fn any_active_group_true(numbers: &[u32], group_is_true: impl FnMut(usize) -> bool) -> bool {
    active_group_indices(numbers).into_iter().any(group_is_true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fully_contiguous_keeps_all_in_ascending_order() {
        assert_eq!(active_group_indices(&[1, 2, 3]), vec![0, 1, 2]);
    }

    #[test]
    fn non_contiguous_group_truncates_at_the_gap() {
        // The headline acceptance case: `trigger1, trigger2, trigger4` (no
        // `trigger3`) — group 4 is dead, exactly like MUGEN.
        assert_eq!(active_group_indices(&[1, 2, 4]), vec![0, 1]);
        // A gap right after trigger1 kills everything beyond it.
        assert_eq!(active_group_indices(&[1, 3, 4]), vec![0]);
    }

    #[test]
    fn missing_trigger1_yields_nothing() {
        assert!(active_group_indices(&[2, 3]).is_empty());
        assert!(active_group_indices(&[]).is_empty());
    }

    #[test]
    fn input_order_is_irrelevant_indices_map_back() {
        // Numbers out of order: indices must still point into the input slice.
        assert_eq!(active_group_indices(&[3, 1, 2]), vec![1, 2, 0]);
        assert_eq!(active_group_indices(&[2, 1, 4]), vec![1, 0]);
    }

    #[test]
    fn duplicate_numbers_do_not_break_contiguity() {
        // Two `trigger1` entries plus a `trigger2`: contiguity holds; the
        // duplicate collapses to the first index for that number.
        let active = active_group_indices(&[1, 1, 2]);
        assert_eq!(active, vec![0, 2]);
    }

    #[test]
    fn any_active_group_true_ors_over_the_contiguous_prefix() {
        // group1 false, group2 true -> fires.
        assert!(any_active_group_true(&[1, 2], |i| [false, true][i]));
        // both active groups false -> does not fire.
        assert!(!any_active_group_true(&[1, 2], |i| [false, false][i]));
    }

    #[test]
    fn any_active_group_true_ignores_groups_past_a_gap() {
        // The only true group is `trigger4`, which is dead (gap at 3).
        let truth = [false, false, true];
        assert!(!any_active_group_true(&[1, 2, 4], |i| truth[i]));
    }

    #[test]
    fn any_active_group_true_short_circuits_and_never_evals_dead_groups() {
        // group1 is true, so the OR stops immediately; the closure must never be
        // asked about the dead trigger3 (index 1).
        let mut visited = [false; 2];
        let fired = any_active_group_true(&[1, 3], |i| {
            visited[i] = true;
            true
        });
        assert!(fired);
        assert!(visited[0], "the active trigger1 group must be evaluated");
        assert!(
            !visited[1],
            "the dead trigger3 group must never be evaluated"
        );
    }
}
