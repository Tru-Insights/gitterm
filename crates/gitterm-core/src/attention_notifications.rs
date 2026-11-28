//! Background needs-you policy shared by desktop frontends.
//! Hosts supply observations and a monotonic clock; observing never posts.
use std::{
    collections::{HashMap, HashSet},
    time::{Duration, Instant},
};

pub const INPUT_REQUIRED: &str = "Input or approval needed";
pub const WORKER_BLOCKED: &str = "Worker blocked";
pub const DELEGATION_BLOCKED: &str = "Delegation blocked";

/// One look at something that can need the human, fed to
/// [`NeedsYouNotifier::observe`] (TRU-148).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NeedsYouObservation<T> {
    pub target: T,
    /// The needs-you reason's label while it waits on the human.
    pub needs_you: Option<&'static str>,
    /// Mid-turn right now.
    pub running: bool,
    /// In front of the user (the active tab of the active workspace).
    pub front: bool,
    /// Its needs-you only counts after it was seen working: a harness
    /// title's "✳" also means idle at the prompt, which a restored tab
    /// shows on boot without anything having happened.
    pub requires_work: bool,
}

/// A needs-you transition worth a notification: where, and why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NeedsYouNudge<T> {
    pub target: T,
    pub label: &'static str,
}

/// Detects background tabs and tasks turning to needs-you, for the macOS
/// notification (TRU-148). Level-triggered: each observation is compared
/// with the last, so a reason that stays raised never repeats; one that
/// clears and comes back is a new transition, held to one notification
/// per target and reason every [`Self::REPEAT_AFTER`].
#[derive(Debug)]
pub struct NeedsYouNotifier<T> {
    started: Instant,
    raised: HashSet<(T, &'static str)>,
    seen_working: HashSet<T>,
    last_fired: HashMap<(T, &'static str), Instant>,
}

impl<T: Clone + Eq + std::hash::Hash> NeedsYouNotifier<T> {
    /// Restored tabs settle (titles, requests, task states) in the first
    /// seconds after launch; nothing then is news.
    const LAUNCH_GRACE: Duration = Duration::from_secs(5);
    const REPEAT_AFTER: Duration = Duration::from_secs(60);

    pub fn new(started: Instant) -> Self {
        Self {
            started,
            raised: HashSet::new(),
            seen_working: HashSet::new(),
            last_fired: HashMap::new(),
        }
    }

    /// Record what is raised now and return the transitions to announce:
    /// newly raised, not in front, past the launch grace, worked first when
    /// that is required, and not announced for the same reason within
    /// [`Self::REPEAT_AFTER`].
    pub fn observe(
        &mut self,
        now: Instant,
        enabled: bool,
        observations: impl IntoIterator<Item = NeedsYouObservation<T>>,
    ) -> Vec<NeedsYouNudge<T>> {
        if !enabled {
            return Vec::new();
        }
        let in_grace = now.saturating_duration_since(self.started) < Self::LAUNCH_GRACE;
        self.last_fired
            .retain(|_, fired| now.saturating_duration_since(*fired) < Self::REPEAT_AFTER);
        let mut raised = HashSet::new();
        let mut nudges = Vec::new();
        for observation in observations {
            if observation.running {
                self.seen_working.insert(observation.target.clone());
            }
            let Some(label) = observation.needs_you else {
                continue;
            };
            let key = (observation.target.clone(), label);
            raised.insert(key.clone());
            if self.raised.contains(&key) {
                continue;
            }
            // A transition spends the working mark, announced or not.
            let worked = self.seen_working.remove(&observation.target);
            if in_grace
                || observation.front
                || (observation.requires_work && !worked)
                || self.last_fired.contains_key(&key)
            {
                continue;
            }
            self.last_fired.insert(key, now);
            nudges.push(NeedsYouNudge {
                target: observation.target,
                label,
            });
        }
        self.raised = raised;
        nudges
    }
}

/// `text` as an AppleScript string literal.
pub fn applescript_string(text: &str) -> String {
    let mut literal = String::with_capacity(text.len() + 2);
    literal.push('"');
    for ch in text.chars() {
        match ch {
            '"' => literal.push_str("\\\""),
            '\\' => literal.push_str("\\\\"),
            '\n' | '\r' => literal.push(' '),
            _ => literal.push(ch),
        }
    }
    literal.push('"');
    literal
}

/// The `osascript` source that posts a needs-you notification.
pub fn needs_you_notification_script(title: &str, body: &str) -> String {
    format!(
        "display notification {} with title {}",
        applescript_string(body),
        applescript_string(title)
    )
}

/// Post a macOS notification off the UI thread. A plain notification:
/// `osascript` owns it, so clicking it cannot focus GitTerm's tab.
pub fn post_needs_you_notification(title: &str, body: &str) {
    post_notification_script(needs_you_notification_script(title, body));
}

/// Deliver an already escaped script using Iced's original mechanism.
pub fn post_notification_script(script: String) {
    std::thread::spawn(move || {
        match std::process::Command::new("osascript")
            .arg("-e")
            .arg(&script)
            .output()
        {
            Ok(output) if output.status.success() => {}
            Ok(output) => eprintln!(
                "[notify] osascript exited {}: {}",
                output.status,
                String::from_utf8_lossy(&output.stderr).trim()
            ),
            Err(error) => eprintln!("[notify] could not run osascript: {error}"),
        }
    });
}

/// Whether a harness title says it is working: the leading glyph is one of
/// the spinner frames [`strip_title_status_glyphs`] documents ("◐"/"◑", the
/// older sparkle set, braille spinners) — not the idle "✳" or a shell's "*".
pub fn terminal_title_shows_working(title: &str) -> bool {
    title.trim_start().chars().next().is_some_and(|ch| {
        matches!(ch, '◐' | '◑' | '◒' | '◓' | '✢' | '✶' | '✻' | '✽' | '·')
            || ('\u{2800}'..='\u{28FF}').contains(&ch)
    })
}

/// A harness title with its leading status glyphs removed. Claude Code
/// writes "✳ <topic>" while it is not working and alternates "◐ <topic>" /
/// "◑ <topic>" every ~1s while it is; older builds and other harnesses use
/// the "·✢✶✻✽" sparkle set or braille spinners, and shells may prefix "*".
/// The topic after the glyph is what a row or progress line should show —
/// the glyph changes every frame and would make the text churn.
pub fn strip_title_status_glyphs(title: &str) -> &str {
    title
        .trim_start_matches(|ch: char| {
            matches!(
                ch,
                '✳' | '✢' | '✶' | '✻' | '✽' | '·' | '◐' | '◑' | '◒' | '◓' | '*'
            ) || ('\u{2800}'..='\u{28FF}').contains(&ch)
                || ch.is_whitespace()
        })
        .trim_end()
}

#[cfg(test)]
mod tests {
    use super::*;
    fn input(target: u8) -> NeedsYouObservation<u8> {
        NeedsYouObservation {
            target,
            needs_you: Some(INPUT_REQUIRED),
            running: false,
            front: false,
            requires_work: false,
        }
    }
    #[test]
    fn attention_notifications_grace_consumes_restored_edges_and_ends_at_five_seconds() {
        let start = Instant::now();
        let mut notifier = NeedsYouNotifier::new(start);
        assert!(notifier
            .observe(start + Duration::from_secs(4), true, [input(1)])
            .is_empty());
        assert_eq!(
            notifier.observe(start + Duration::from_secs(5), true, [input(1), input(2)]),
            [NeedsYouNudge {
                target: 2,
                label: INPUT_REQUIRED
            }]
        );
    }
    #[test]
    fn attention_notifications_repeat_requires_a_new_edge_and_sixty_seconds() {
        let start = Instant::now();
        let at = |s| start + Duration::from_secs(s);
        let mut notifier = NeedsYouNotifier::new(start);
        assert_eq!(notifier.observe(at(5), true, [input(1)]).len(), 1);
        assert!(notifier.observe(at(100), true, [input(1)]).is_empty());
        notifier.observe(at(101), true, []);
        assert_eq!(notifier.observe(at(102), true, [input(1)]).len(), 1);
        notifier.observe(at(103), true, []);
        assert!(notifier.observe(at(161), true, [input(1)]).is_empty());
        notifier.observe(at(161), true, []);
        assert_eq!(notifier.observe(at(162), true, [input(1)]).len(), 1);
    }
    #[test]
    fn attention_notifications_front_edge_is_consumed_even_after_switching_away() {
        let start = Instant::now();
        let mut notifier = NeedsYouNotifier::new(start);
        let mut front = input(1);
        front.front = true;
        assert!(notifier
            .observe(start + Duration::from_secs(5), true, [front])
            .is_empty());
        assert!(notifier
            .observe(start + Duration::from_secs(6), true, [input(1)])
            .is_empty());
    }
    #[test]
    fn attention_notifications_setting_off_does_not_observe_or_spend_edges() {
        let start = Instant::now();
        let mut notifier = NeedsYouNotifier::new(start);
        assert!(notifier
            .observe(start + Duration::from_secs(5), false, [input(1)])
            .is_empty());
        assert_eq!(
            notifier
                .observe(start + Duration::from_secs(6), true, [input(1)])
                .len(),
            1
        );
        assert!(notifier
            .observe(start + Duration::from_secs(7), false, [])
            .is_empty());
        assert!(notifier
            .observe(start + Duration::from_secs(70), true, [input(1)])
            .is_empty());
    }
    #[test]
    fn attention_notifications_terminal_idle_requires_fresh_work() {
        let start = Instant::now();
        let at = |s| start + Duration::from_secs(s);
        let mut notifier = NeedsYouNotifier::new(start);
        let mut terminal = input(1);
        terminal.requires_work = true;
        assert!(notifier.observe(at(5), true, [terminal.clone()]).is_empty());
        let mut working = terminal.clone();
        working.needs_you = None;
        working.running = true;
        notifier.observe(at(6), true, [working]);
        assert_eq!(notifier.observe(at(7), true, [terminal.clone()]).len(), 1);
        notifier.observe(at(70), true, []);
        assert!(notifier.observe(at(71), true, [terminal]).is_empty());
    }
    #[test]
    fn attention_notifications_worker_blocked_once_and_separate_from_input() {
        let start = Instant::now();
        let mut notifier = NeedsYouNotifier::new(start);
        notifier.observe(start + Duration::from_secs(5), true, [input(1)]);
        let mut blocked = input(1);
        blocked.needs_you = Some(WORKER_BLOCKED);
        assert_eq!(
            notifier.observe(start + Duration::from_secs(6), true, [blocked.clone()]),
            [NeedsYouNudge {
                target: 1,
                label: WORKER_BLOCKED
            }]
        );
        assert!(notifier
            .observe(start + Duration::from_secs(100), true, [blocked])
            .is_empty());
    }
    #[test]
    fn attention_notifications_escaping_is_iced_applescript_bytes() {
        assert_eq!(
            needs_you_notification_script("Fix \"fixture\"\\path\nnext", "Worker\rblocked"),
            r#"display notification "Worker blocked" with title "Fix \"fixture\"\\path next""#
        );
        assert_eq!(applescript_string("雪\tfixture"), "\"雪\tfixture\"");
    }
}
