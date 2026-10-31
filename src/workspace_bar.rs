//! Display order of the bottom workspace bar (TRU-175): work first, repos
//! after. Inside each machine group the pieces of work (workspaces that
//! own or claim chats) come first in their existing relative order, then a
//! `Repos` toggle, then the plain folder workspaces. Collapsing the toggle
//! hides the folders except the active one, which stays visible after the
//! toggle. Folding the whole machine group hides every chip but the active
//! one, as before.
//!
//! This is display order only: `App::workspaces` keeps its order, so
//! persisted indices and `active_workspace` are untouched. The bar,
//! Ctrl+1-9, the left spine's dots, the attention jump
//! ([`next_attention`]) and the content slide ([`slide_order`]) all read
//! [`visible_order`], so the nth key, dot and panel is the nth chip.

use std::collections::HashSet;

/// What the layout needs to know about one workspace, in `App::workspaces`
/// order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct BarChip {
    /// The machine group key ("local", "agent:<id>", "ssh:<host>").
    pub group: String,
    /// A piece of work rather than a plain folder workspace.
    pub work: bool,
}

/// The `Repos` toggle of one machine group: present when the group has
/// folder workspaces and is not folded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ReposToggle {
    /// Folder workspaces in the group.
    pub count: usize,
    pub collapsed: bool,
    /// Folder workspaces the collapse hides (never the active one).
    pub hidden: Vec<usize>,
}

/// One machine group of the bar, chips as `App::workspaces` indices.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct BarGroup {
    pub key: String,
    /// Workspaces in the group, shown on a folded machine label.
    pub size: usize,
    /// The machine group is folded.
    pub folded: bool,
    /// Visible chips before the toggle: the work.
    pub work: Vec<usize>,
    pub repos: Option<ReposToggle>,
    /// Visible chips after the toggle: the folders.
    pub folders: Vec<usize>,
}

/// The bar's groups in order of first appearance. `folded_machines` and
/// `collapsed_repos` hold machine group keys.
pub(crate) fn bar_layout(
    chips: &[BarChip],
    active: usize,
    folded_machines: &HashSet<String>,
    collapsed_repos: &HashSet<String>,
) -> Vec<BarGroup> {
    layout_pinned(chips, &[active], folded_machines, collapsed_repos)
}

/// The content slide's workspaces left to right: the visible order, also
/// keeping `from` (the workspace a slide or swipe started from) in place
/// until the slide settles, so the panels do not shift under it.
pub(crate) fn slide_order(
    chips: &[BarChip],
    active: usize,
    from: Option<usize>,
    folded_machines: &HashSet<String>,
    collapsed_repos: &HashSet<String>,
) -> Vec<usize> {
    let pinned: Vec<usize> = std::iter::once(active).chain(from).collect();
    visible_order(&layout_pinned(
        chips,
        &pinned,
        folded_machines,
        collapsed_repos,
    ))
}

/// `bar_layout` where every index in `pinned` stays visible the way the
/// active workspace does.
fn layout_pinned(
    chips: &[BarChip],
    pinned: &[usize],
    folded_machines: &HashSet<String>,
    collapsed_repos: &HashSet<String>,
) -> Vec<BarGroup> {
    let mut groups: Vec<BarGroup> = Vec::new();
    for (idx, chip) in chips.iter().enumerate() {
        let pos = match groups.iter().position(|group| group.key == chip.group) {
            Some(pos) => pos,
            None => {
                groups.push(BarGroup {
                    key: chip.group.clone(),
                    size: 0,
                    folded: folded_machines.contains(&chip.group),
                    work: Vec::new(),
                    repos: None,
                    folders: Vec::new(),
                });
                groups.len() - 1
            }
        };
        let group = &mut groups[pos];
        group.size += 1;
        let is_active = pinned.contains(&idx);
        if group.folded {
            if is_active {
                if chip.work {
                    group.work.push(idx);
                } else {
                    group.folders.push(idx);
                }
            }
            continue;
        }
        if chip.work {
            group.work.push(idx);
            continue;
        }
        let toggle = group.repos.get_or_insert_with(|| ReposToggle {
            count: 0,
            collapsed: collapsed_repos.contains(&chip.group),
            hidden: Vec::new(),
        });
        toggle.count += 1;
        if toggle.collapsed && !is_active {
            toggle.hidden.push(idx);
        } else {
            group.folders.push(idx);
        }
    }
    groups
}

/// The visible chips left to right, as `App::workspaces` indices: the
/// order Ctrl+1-9 follows.
pub(crate) fn visible_order(groups: &[BarGroup]) -> Vec<usize> {
    groups
        .iter()
        .flat_map(|group| group.work.iter().chain(&group.folders).copied())
        .collect()
}

/// The next tab needing attention after the active one, as (workspace,
/// tab): the active workspace's later tabs, then each workspace after it
/// in `order` (wrapping), then the active workspace's earlier tabs and
/// the active tab itself. `tab_count(ws)` and `needs(ws, tab)` read the
/// app. A workspace missing from `order` is never visited, except the
/// active one.
pub(crate) fn next_attention(
    order: &[usize],
    active: usize,
    active_tab: usize,
    tab_count: impl Fn(usize) -> usize,
    needs: impl Fn(usize, usize) -> bool,
) -> Option<(usize, usize)> {
    let later = (active_tab + 1..tab_count(active)).map(|tab| (active, tab));
    let start = order.iter().position(|&idx| idx == active);
    let others: Vec<usize> = match start {
        Some(pos) => order[pos + 1..]
            .iter()
            .chain(&order[..pos])
            .copied()
            .collect(),
        None => order.to_vec(),
    };
    let elsewhere = others
        .into_iter()
        .flat_map(|ws| (0..tab_count(ws)).map(move |tab| (ws, tab)));
    let earlier = (0..=active_tab)
        .filter(|&tab| tab < tab_count(active))
        .map(|tab| (active, tab));
    later
        .chain(elsewhere)
        .chain(earlier)
        .find(|&(ws, tab)| needs(ws, tab))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chip(group: &str, work: bool) -> BarChip {
        BarChip {
            group: group.to_string(),
            work,
        }
    }

    fn set(keys: &[&str]) -> HashSet<String> {
        keys.iter().map(|key| key.to_string()).collect()
    }

    #[test]
    fn work_comes_before_folders_in_relative_order() {
        // folder-a, work-x, folder-b, work-y on this mac.
        let chips = [
            chip("local", false),
            chip("local", true),
            chip("local", false),
            chip("local", true),
        ];
        let groups = bar_layout(&chips, 0, &set(&[]), &set(&[]));
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].work, [1, 3]);
        assert_eq!(groups[0].folders, [0, 2]);
        assert_eq!(
            groups[0].repos,
            Some(ReposToggle {
                count: 2,
                collapsed: false,
                hidden: Vec::new(),
            })
        );
        assert_eq!(visible_order(&groups), [1, 3, 0, 2]);
    }

    #[test]
    fn collapsed_repos_keep_the_active_folder_visible() {
        let chips = [
            chip("local", false),
            chip("local", true),
            chip("local", false),
            chip("local", false),
        ];
        let groups = bar_layout(&chips, 2, &set(&[]), &set(&["local"]));
        let toggle = groups[0].repos.as_ref().unwrap();
        assert!(toggle.collapsed);
        assert_eq!(toggle.count, 3);
        assert_eq!(toggle.hidden, [0, 3]);
        assert_eq!(groups[0].folders, [2]);
        assert_eq!(visible_order(&groups), [1, 2]);

        // With work active, every folder hides.
        let groups = bar_layout(&chips, 1, &set(&[]), &set(&["local"]));
        assert_eq!(groups[0].repos.as_ref().unwrap().hidden, [0, 2, 3]);
        assert_eq!(visible_order(&groups), [1]);
    }

    #[test]
    fn groups_without_folders_have_no_toggle() {
        let chips = [chip("local", true), chip("local", true)];
        let groups = bar_layout(&chips, 0, &set(&[]), &set(&["local"]));
        assert_eq!(groups[0].repos, None);
        assert_eq!(visible_order(&groups), [0, 1]);
    }

    #[test]
    fn each_machine_group_orders_and_collapses_on_its_own() {
        // Vec order is machine-normalized: this mac, then two remotes.
        let chips = [
            chip("local", false),
            chip("local", true),
            chip("agent:box-one", false),
            chip("agent:box-one", true),
            chip("agent:box-one", false),
            chip("agent:box-two", false),
            chip("agent:box-two", true),
        ];
        let groups = bar_layout(&chips, 4, &set(&[]), &set(&["agent:box-one"]));
        let keys: Vec<&str> = groups.iter().map(|group| group.key.as_str()).collect();
        assert_eq!(keys, ["local", "agent:box-one", "agent:box-two"]);
        assert_eq!(groups[0].folders, [0]);
        assert_eq!(groups[1].repos.as_ref().unwrap().hidden, [2]);
        assert_eq!(groups[1].folders, [4]);
        assert!(!groups[2].repos.as_ref().unwrap().collapsed);
        assert_eq!(visible_order(&groups), [1, 0, 3, 4, 6, 5]);
        assert_eq!(
            groups.iter().map(|group| group.size).collect::<Vec<_>>(),
            [2, 3, 2]
        );
    }

    #[test]
    fn a_folded_machine_shows_only_its_active_chip_and_no_toggle() {
        let chips = [
            chip("local", true),
            chip("agent:box-one", true),
            chip("agent:box-one", false),
            chip("agent:box-one", false),
        ];
        let groups = bar_layout(&chips, 3, &set(&["agent:box-one"]), &set(&[]));
        assert!(groups[1].folded);
        assert_eq!(groups[1].repos, None);
        assert!(groups[1].work.is_empty());
        assert_eq!(groups[1].folders, [3]);
        assert_eq!(groups[1].size, 3);
        assert_eq!(visible_order(&groups), [0, 3]);

        // Active elsewhere: the folded group shows no chips.
        let groups = bar_layout(&chips, 0, &set(&["agent:box-one"]), &set(&[]));
        assert_eq!(visible_order(&groups), [0]);
    }

    #[test]
    fn the_slide_keeps_its_starting_workspace_until_it_settles() {
        let chips = [
            chip("local", false),
            chip("local", true),
            chip("local", false),
            chip("agent:box-one", true),
        ];
        let collapsed = set(&["local"]);
        // Settled on hidden folder 2: it shows after the work.
        assert_eq!(
            slide_order(&chips, 2, None, &set(&[]), &collapsed),
            [1, 2, 3]
        );
        // Sliding from 2 to 3: 2 stays in place until the slide settles.
        assert_eq!(
            slide_order(&chips, 3, Some(2), &set(&[]), &collapsed),
            [1, 2, 3]
        );
        // Settled on 3: the collapsed folders leave the slide.
        assert_eq!(slide_order(&chips, 3, None, &set(&[]), &collapsed), [1, 3]);
        // Sliding to a hidden folder from work: both stay visible.
        assert_eq!(
            slide_order(&chips, 0, Some(3), &set(&[]), &collapsed),
            [1, 0, 3]
        );
    }

    #[test]
    fn next_attention_follows_the_visible_order_and_wraps() {
        // Visible order [1, 0, 3]; workspace 2 is hidden.
        let order = [1, 0, 3];
        let tabs = |ws: usize| [2, 3, 1, 2][ws];
        let waiting = |marks: &'static [(usize, usize)]| {
            move |ws: usize, tab: usize| marks.contains(&(ws, tab))
        };
        // From workspace 1 tab 0: its own tab 1 first.
        assert_eq!(
            next_attention(&order, 1, 0, tabs, waiting(&[(1, 1), (0, 0)])),
            Some((1, 1))
        );
        // Then the next visible workspace, not the next index.
        assert_eq!(
            next_attention(&order, 1, 2, tabs, waiting(&[(3, 1), (0, 1)])),
            Some((0, 1))
        );
        // Hidden workspaces are skipped.
        assert_eq!(next_attention(&order, 1, 2, tabs, waiting(&[(2, 0)])), None);
        // Wraps past the end back to the active workspace's earlier tabs.
        assert_eq!(
            next_attention(&order, 3, 1, tabs, waiting(&[(3, 0)])),
            Some((3, 0))
        );
        assert_eq!(
            next_attention(&order, 3, 1, tabs, waiting(&[(1, 0), (3, 0)])),
            Some((1, 0))
        );
        // The active tab itself is found last.
        assert_eq!(
            next_attention(&order, 0, 1, tabs, waiting(&[(0, 1)])),
            Some((0, 1))
        );
    }
}
