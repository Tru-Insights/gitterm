//! Display order of the bottom workspace bar (TRU-175, TRU-192): this
//! Mac's work first in its existing relative order, then the `Repos ▾`
//! menu, then this Mac's folder workspaces when they are shown on the bar.
//! Other machines' workspaces live in the menu, one section per machine;
//! the active workspace always stays on the bar, after the folders, so it
//! never disappears out from under the user.
//!
//! This is display order only: `App::workspaces` keeps its order, so
//! persisted indices and `active_workspace` are untouched. The bar,
//! Ctrl+1-9, the left spine's dots, the attention jump
//! ([`next_attention`]) and the content slide ([`slide_order`]) all read
//! [`visible_order`], so the nth key, dot and panel is the nth chip.

/// The machine group key of this Mac's workspaces.
pub(crate) const LOCAL_GROUP: &str = "local";

/// What the layout needs to know about one workspace, in `App::workspaces`
/// order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct BarChip {
    /// The machine group key ("local", "agent:<id>", "ssh:<host>").
    pub group: String,
    /// A piece of work rather than a plain folder workspace.
    pub work: bool,
    /// Parked while it keeps running (TRU-186): off the bar, the menu, the
    /// slide and Ctrl+1-9 entirely, even when pinned.
    pub parked: bool,
}

/// One machine's section of the `Repos ▾` menu.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct MachineSection {
    pub key: String,
    /// The workspaces the section lists: this Mac's folders, or every
    /// workspace of another machine, work first.
    pub workspaces: Vec<usize>,
}

/// The bar's chips as `App::workspaces` indices, and the `Repos ▾` menu.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct BarLayout {
    /// Chips before the menu: this Mac's work.
    pub work: Vec<usize>,
    /// Chips after the menu: this Mac's folders when they are on the bar,
    /// then a pinned (active) workspace only the menu would list.
    pub after: Vec<usize>,
    /// The menu's sections, this Mac first (always present, maybe empty),
    /// then the other machines in order of first appearance.
    pub sections: Vec<MachineSection>,
    /// Workspaces only the menu shows, never a pinned one: the menu
    /// button's news dot reads these.
    pub hidden: Vec<usize>,
}

/// The bar's layout. `repos_on_bar` shows this Mac's folder workspaces on
/// the bar as well as in the menu.
pub(crate) fn bar_layout(chips: &[BarChip], active: usize, repos_on_bar: bool) -> BarLayout {
    layout_pinned(chips, &[active], repos_on_bar)
}

/// The content slide's workspaces left to right: the visible order, also
/// keeping `from` (the workspace a slide or swipe started from) in place
/// until the slide settles, so the panels do not shift under it.
pub(crate) fn slide_order(
    chips: &[BarChip],
    active: usize,
    from: Option<usize>,
    repos_on_bar: bool,
) -> Vec<usize> {
    let pinned: Vec<usize> = std::iter::once(active).chain(from).collect();
    visible_order(&layout_pinned(chips, &pinned, repos_on_bar))
}

/// `bar_layout` where every index in `pinned` stays visible the way the
/// active workspace does.
fn layout_pinned(chips: &[BarChip], pinned: &[usize], repos_on_bar: bool) -> BarLayout {
    let mut layout = BarLayout {
        work: Vec::new(),
        after: Vec::new(),
        sections: vec![MachineSection {
            key: LOCAL_GROUP.to_string(),
            workspaces: Vec::new(),
        }],
        hidden: Vec::new(),
    };
    // Other machines list their work first, then their folders.
    let mut remote_folders: Vec<(String, usize)> = Vec::new();
    for (idx, chip) in chips.iter().enumerate() {
        if chip.parked {
            continue;
        }
        let local = chip.group == LOCAL_GROUP;
        if local && chip.work {
            layout.work.push(idx);
            continue;
        }
        if pinned.contains(&idx) || (local && repos_on_bar) {
            layout.after.push(idx);
        } else {
            layout.hidden.push(idx);
        }
        if local || chip.work {
            section_of(&mut layout.sections, &chip.group)
                .workspaces
                .push(idx);
        } else {
            section_of(&mut layout.sections, &chip.group);
            remote_folders.push((chip.group.clone(), idx));
        }
    }
    for (group, idx) in remote_folders {
        section_of(&mut layout.sections, &group)
            .workspaces
            .push(idx);
    }
    layout
}

fn section_of<'a>(sections: &'a mut Vec<MachineSection>, key: &str) -> &'a mut MachineSection {
    let pos = match sections.iter().position(|section| section.key == key) {
        Some(pos) => pos,
        None => {
            sections.push(MachineSection {
                key: key.to_string(),
                workspaces: Vec::new(),
            });
            sections.len() - 1
        }
    };
    &mut sections[pos]
}

/// The visible chips left to right, as `App::workspaces` indices: the
/// order Ctrl+1-9 follows.
pub(crate) fn visible_order(layout: &BarLayout) -> Vec<usize> {
    layout.work.iter().chain(&layout.after).copied().collect()
}

/// About how many characters an inactive chip's short label keeps.
pub(crate) const SHORT_LABEL_CHARS: usize = 18;

/// Words a short label drops first.
const FILLER_WORDS: [&str; 11] = [
    "from", "and", "the", "for", "of", "with", "a", "an", "to", "in", "&",
];

/// An inactive chip's label (TRU-192): the name itself when it fits in
/// [`SHORT_LABEL_CHARS`], otherwise the name without filler words, cut at
/// a word boundary (or mid-word when the first word alone is too long)
/// and ended with `…`.
pub(crate) fn short_label(name: &str) -> String {
    let name = name.trim();
    if name.chars().count() <= SHORT_LABEL_CHARS {
        return name.to_string();
    }
    let words: Vec<&str> = name.split_whitespace().collect();
    let mut kept: Vec<&str> = words
        .iter()
        .copied()
        .filter(|word| {
            !FILLER_WORDS
                .iter()
                .any(|filler| word.eq_ignore_ascii_case(filler))
        })
        .collect();
    if kept.is_empty() {
        kept = words;
    }
    let joined = kept.join(" ");
    if joined.chars().count() <= SHORT_LABEL_CHARS {
        return joined;
    }
    // Leave room for the ellipsis.
    let budget = SHORT_LABEL_CHARS - 1;
    let mut cut = String::new();
    for word in &kept {
        let len = cut.chars().count() + usize::from(!cut.is_empty()) + word.chars().count();
        if len > budget {
            break;
        }
        if !cut.is_empty() {
            cut.push(' ');
        }
        cut.push_str(word);
    }
    // A word boundary that keeps less than half the budget reads worse
    // than a cut word.
    if cut.chars().count() < budget / 2 {
        cut = joined.chars().take(budget).collect::<String>();
        cut.truncate(cut.trim_end().len());
    }
    cut.push('\u{2026}');
    cut
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
            parked: false,
        }
    }

    fn section(key: &str, workspaces: &[usize]) -> MachineSection {
        MachineSection {
            key: key.to_string(),
            workspaces: workspaces.to_vec(),
        }
    }

    #[test]
    fn a_running_parked_workspace_is_off_the_bar_the_menu_and_the_slide() {
        let mut chips = [
            chip("local", true),
            chip("local", true),
            chip("agent:box-one", true),
        ];
        chips[1].parked = true;
        chips[2].parked = true;
        let layout = bar_layout(&chips, 0, true);
        // A machine whose only workspaces are parked has no section.
        assert_eq!(layout.sections, [section("local", &[])]);
        assert!(layout.hidden.is_empty());
        assert_eq!(visible_order(&layout), [0]);
        // Even pinned (active or a slide's start), it stays off.
        assert_eq!(slide_order(&chips, 0, Some(1), true), [0]);
        assert_eq!(slide_order(&chips, 0, Some(2), true), [0]);
    }

    #[test]
    fn work_comes_first_then_the_folders_on_the_bar() {
        // folder-a, work-x, folder-b, work-y on this mac.
        let chips = [
            chip("local", false),
            chip("local", true),
            chip("local", false),
            chip("local", true),
        ];
        let layout = bar_layout(&chips, 0, true);
        assert_eq!(layout.work, [1, 3]);
        assert_eq!(layout.after, [0, 2]);
        assert!(layout.hidden.is_empty());
        // The menu lists this Mac's folders, not its work.
        assert_eq!(layout.sections, [section("local", &[0, 2])]);
        assert_eq!(visible_order(&layout), [1, 3, 0, 2]);
    }

    #[test]
    fn repos_off_the_bar_keep_the_active_folder_visible() {
        let chips = [
            chip("local", false),
            chip("local", true),
            chip("local", false),
            chip("local", false),
        ];
        let layout = bar_layout(&chips, 2, false);
        assert_eq!(layout.after, [2]);
        assert_eq!(layout.hidden, [0, 3]);
        assert_eq!(layout.sections, [section("local", &[0, 2, 3])]);
        assert_eq!(visible_order(&layout), [1, 2]);

        // With work active, every folder hides.
        let layout = bar_layout(&chips, 1, false);
        assert_eq!(layout.hidden, [0, 2, 3]);
        assert_eq!(visible_order(&layout), [1]);
    }

    #[test]
    fn this_mac_always_has_a_menu_section() {
        let chips = [chip("local", true), chip("local", true)];
        let layout = bar_layout(&chips, 0, false);
        assert_eq!(layout.sections, [section("local", &[])]);
        assert_eq!(visible_order(&layout), [0, 1]);
        let layout = bar_layout(&[], 0, true);
        assert_eq!(layout.sections, [section("local", &[])]);
        assert!(visible_order(&layout).is_empty());
    }

    #[test]
    fn other_machines_live_in_the_menu_one_section_each() {
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
        let layout = bar_layout(&chips, 1, true);
        assert_eq!(
            layout.sections,
            [
                section("local", &[0]),
                // Work first, then folders.
                section("agent:box-one", &[3, 2, 4]),
                section("agent:box-two", &[6, 5]),
            ]
        );
        assert_eq!(layout.hidden, [2, 3, 4, 5, 6]);
        assert_eq!(visible_order(&layout), [1, 0]);

        // The active remote workspace stays on the bar, after the folders,
        // and is still listed in its section.
        let layout = bar_layout(&chips, 4, true);
        assert_eq!(layout.after, [0, 4]);
        assert_eq!(layout.hidden, [2, 3, 5, 6]);
        assert_eq!(layout.sections[1], section("agent:box-one", &[3, 2, 4]));
        assert_eq!(visible_order(&layout), [1, 0, 4]);
    }

    #[test]
    fn the_slide_keeps_its_starting_workspace_until_it_settles() {
        let chips = [
            chip("local", false),
            chip("local", true),
            chip("local", false),
            chip("agent:box-one", true),
        ];
        // Settled on hidden folder 2: it shows after the work.
        assert_eq!(slide_order(&chips, 2, None, false), [1, 2]);
        // Sliding from 2 to the remote 3: 2 stays in place until the
        // slide settles.
        assert_eq!(slide_order(&chips, 3, Some(2), false), [1, 2, 3]);
        // Settled on 3: the hidden folders leave the slide.
        assert_eq!(slide_order(&chips, 3, None, false), [1, 3]);
        // Sliding to a hidden folder from the remote: both stay visible.
        assert_eq!(slide_order(&chips, 0, Some(3), false), [1, 0, 3]);
    }

    #[test]
    fn short_labels_keep_short_names_and_shorten_long_ones() {
        // Fits: unchanged, filler words and all.
        assert_eq!(short_label("Fixes for the app"), "Fixes for the app");
        assert_eq!(short_label("  api  "), "api");
        // Dropping filler words is enough.
        assert_eq!(short_label("Docs and the release"), "Docs release");
        // Otherwise cut at a word boundary, with an ellipsis.
        assert_eq!(
            short_label("Scanner sizing migration from legacy hosts"),
            "Scanner sizing\u{2026}"
        );
        assert_eq!(
            short_label("Invoicing fixes and enhancements"),
            "Invoicing fixes\u{2026}"
        );
        // Filler words are matched whole and in any case.
        assert_eq!(
            short_label("The Andes FOR offline maps"),
            "Andes offline maps"
        );
        // A first word too long for the budget is cut mid-word.
        assert_eq!(
            short_label("Internationalisation rollout"),
            "Internationalisat\u{2026}"
        );
        // A name of filler words only keeps them.
        let fillers = short_label("the and for of with from a an to");
        assert!(fillers.ends_with('\u{2026}'), "{fillers}");
        // Every derived label fits.
        for name in [
            "Scanner sizing migration from legacy hosts",
            "the and for of with from a an to",
            "\u{dc}n\u{ef}c\u{f6}d\u{e9} w\u{f6}rkspace with a very long name",
            "a b c d e f g h i j k l m n o p q r s t",
            "Internationalisation rollout",
        ] {
            let label = short_label(name);
            assert!(label.chars().count() <= SHORT_LABEL_CHARS, "{label}");
        }
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
