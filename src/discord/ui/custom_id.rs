//! Component `custom_id`s: `cd:1:<bot>:<guild>:<action>[:<arg>][:@<origin>]`.
//!
//! Every button and select the bot posts carries which identity and guild it belongs to, so an id
//! that arrives on the wrong bot (a copied message, a stale controller after a token reshuffle) is
//! refused rather than acted on. The leading `1` is a schema version for the day the format changes.
//! Discord caps custom ids at 100 characters; every action here is well under.

use std::fmt;
use std::str::FromStr;

/// What a component does when pressed or picked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    /// Toggle play/pause.
    PlayPause,
    Skip,
    Previous,
    Stop,
    Shuffle,
    /// Cycle loop mode off → track → queue.
    LoopCycle,
    VolumeUp,
    VolumeDown,
    /// Silence, remembering the volume; press again to bring it back.
    Mute,
    /// Ten seconds back or forward.
    SeekBack,
    SeekForward,
    /// Drop everything queued; the current track plays on.
    Clear,
    /// Leave the voice channel.
    Leave,
    /// Open the queue as a new private message (the controller's Queue button).
    QueueOpen,
    /// Show a queue page (0-based) in place.
    Queue(u32),
    /// Jump to the first / last queue page. Separate from `Queue(n)` so the edge buttons never
    /// share a custom id with their neighbours (Discord refuses duplicates in one message).
    QueueFirst,
    QueueLast,
    /// Open the history as a new private message (the controller's History button).
    HistoryOpen,
    /// `/history` pages, the same way as the queue's.
    History(u32),
    HistoryFirst,
    HistoryLast,
    /// Re-render the now-playing controller.
    Refresh,
    /// Open the current track's lyrics as a private message.
    Lyrics,
    /// Show a lyrics page (0-based) in place.
    LyricsPage(u32),
    /// Jump to the first / last lyrics page; separate so the edge buttons never repeat an id.
    LyricsFirst,
    LyricsLast,
    AutoplayToggle,
    /// Open the equalizer panel as a private message.
    EqOpen,
    /// Nudge the panel's chosen band by this many dB.
    EqStep(i8),
    /// Every band back to zero.
    EqFlat,
    /// The equalizer on or off.
    EqToggle,
    /// A select menu; the arg names which picker it belongs to (`search`, `album`, `artist`, …).
    Select(String),
    /// Confirm a pending destructive action, by nonce.
    Confirm(String),
    Cancel,
    /// A settings toggle, by setting name.
    Setting(String),
    /// A pick from a search result list: play this track id now / add it.
    Play(String),
}

impl Action {
    fn code(&self) -> String {
        match self {
            Action::PlayPause => "pl".into(),
            Action::Skip => "sk".into(),
            Action::Previous => "pv".into(),
            Action::Stop => "st".into(),
            Action::Shuffle => "sh".into(),
            Action::LoopCycle => "lp".into(),
            Action::VolumeUp => "vu".into(),
            Action::VolumeDown => "vd".into(),
            Action::Mute => "mu".into(),
            Action::SeekBack => "sb".into(),
            Action::SeekForward => "sf".into(),
            Action::Clear => "cl".into(),
            Action::Leave => "lv".into(),
            Action::QueueOpen => "qo".into(),
            Action::Queue(p) => format!("q:{p}"),
            Action::QueueFirst => "qf".into(),
            Action::QueueLast => "ql".into(),
            Action::HistoryOpen => "ho".into(),
            Action::History(p) => format!("h:{p}"),
            Action::HistoryFirst => "hf".into(),
            Action::HistoryLast => "hl".into(),
            Action::Refresh => "np".into(),
            Action::Lyrics => "ly".into(),
            Action::LyricsPage(p) => format!("lyp:{p}"),
            Action::LyricsFirst => "lyf".into(),
            Action::LyricsLast => "lyl".into(),
            Action::AutoplayToggle => "ap".into(),
            Action::EqOpen => "eo".into(),
            Action::EqStep(n) => format!("eqs:{n}"),
            Action::EqFlat => "eqf".into(),
            Action::EqToggle => "eqt".into(),
            Action::Select(ctx) => format!("sel:{ctx}"),
            Action::Confirm(n) => format!("cf:{n}"),
            Action::Cancel => "cx".into(),
            Action::Setting(s) => format!("ss:{s}"),
            Action::Play(id) => format!("py:{id}"),
        }
    }

    fn parse(code: &str, arg: Option<&str>) -> Option<Self> {
        Some(match (code, arg) {
            ("pl", None) => Action::PlayPause,
            ("sk", None) => Action::Skip,
            ("pv", None) => Action::Previous,
            ("st", None) => Action::Stop,
            ("sh", None) => Action::Shuffle,
            ("lp", None) => Action::LoopCycle,
            ("vu", None) => Action::VolumeUp,
            ("vd", None) => Action::VolumeDown,
            ("mu", None) => Action::Mute,
            ("sb", None) => Action::SeekBack,
            ("sf", None) => Action::SeekForward,
            ("cl", None) => Action::Clear,
            ("lv", None) => Action::Leave,
            ("qo", None) => Action::QueueOpen,
            ("q", Some(p)) => Action::Queue(p.parse().ok()?),
            ("qf", None) => Action::QueueFirst,
            ("ql", None) => Action::QueueLast,
            ("ho", None) => Action::HistoryOpen,
            ("h", Some(p)) => Action::History(p.parse().ok()?),
            ("hf", None) => Action::HistoryFirst,
            ("hl", None) => Action::HistoryLast,
            ("np", None) => Action::Refresh,
            ("ly", None) => Action::Lyrics,
            ("lyp", Some(p)) => Action::LyricsPage(p.parse().ok()?),
            ("lyf", None) => Action::LyricsFirst,
            ("lyl", None) => Action::LyricsLast,
            ("ap", None) => Action::AutoplayToggle,
            ("eo", None) => Action::EqOpen,
            ("eqs", Some(n)) => Action::EqStep(n.parse().ok()?),
            ("eqf", None) => Action::EqFlat,
            ("eqt", None) => Action::EqToggle,
            ("sel", Some(c)) => Action::Select(c.to_string()),
            ("cf", Some(n)) => Action::Confirm(n.to_string()),
            ("cx", None) => Action::Cancel,
            ("ss", Some(s)) => Action::Setting(s.to_string()),
            ("py", Some(id)) => Action::Play(id.to_string()),
            _ => return None,
        })
    }
}

/// The message a control sits on when it is not the controller (a page of the queue, the
/// history or the lyrics): that message is redrawn after the press, since what it shows has just
/// changed. Carried as a trailing `@q2` / `@h0` / `@l1`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Origin {
    Queue(u32),
    History(u32),
    Lyrics(u32),
}

impl Origin {
    fn code(self) -> String {
        match self {
            Origin::Queue(p) => format!("@q{p}"),
            Origin::History(p) => format!("@h{p}"),
            Origin::Lyrics(p) => format!("@l{p}"),
        }
    }

    fn parse(s: &str) -> Option<Self> {
        let mut rest = s.strip_prefix('@')?.chars();
        let kind = rest.next()?;
        let page: u32 = rest.as_str().parse().ok()?;
        Some(match kind {
            'q' => Origin::Queue(page),
            'h' => Origin::History(page),
            'l' => Origin::Lyrics(page),
            _ => return None,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CustomId {
    /// Identity index in the token list.
    pub bot: u8,
    pub guild: u64,
    pub action: Action,
    /// Set on a control placed on a queue, history or lyrics message.
    pub origin: Option<Origin>,
}

impl CustomId {
    pub fn new(bot: u8, guild: u64, action: Action) -> Self {
        Self {
            bot,
            guild,
            action,
            origin: None,
        }
    }

    /// Note which message the component sits on.
    pub fn on(mut self, origin: Option<Origin>) -> Self {
        self.origin = origin;
        self
    }
}

impl fmt::Display for CustomId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "cd:1:{}:{}:{}", self.bot, self.guild, self.action.code())?;
        if let Some(o) = self.origin {
            write!(f, ":{}", o.code())?;
        }
        Ok(())
    }
}

impl FromStr for CustomId {
    type Err = ();

    fn from_str(s: &str) -> Result<Self, ()> {
        let mut parts: Vec<&str> = s.split(':').collect();
        if parts.len() < 5 || parts[0] != "cd" || parts[1] != "1" {
            return Err(());
        }
        let origin = match parts.last() {
            Some(last) if last.starts_with('@') => {
                let o = Origin::parse(last).ok_or(())?;
                parts.pop();
                Some(o)
            }
            _ => None,
        };
        let bot = parts[2].parse().map_err(|_| ())?;
        let guild = parts[3].parse().map_err(|_| ())?;
        let code = parts[4];
        // An argument keeps any colons of its own.
        let arg = (parts.len() > 5).then(|| parts[5..].join(":"));
        let action = Action::parse(code, arg.as_deref()).ok_or(())?;
        Ok(CustomId {
            bot,
            guild,
            action,
            origin,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_every_action() {
        let actions = vec![
            Action::PlayPause,
            Action::Skip,
            Action::Previous,
            Action::Stop,
            Action::Shuffle,
            Action::LoopCycle,
            Action::VolumeUp,
            Action::VolumeDown,
            Action::Mute,
            Action::SeekBack,
            Action::SeekForward,
            Action::Clear,
            Action::Leave,
            Action::QueueOpen,
            Action::HistoryOpen,
            Action::Queue(3),
            Action::QueueFirst,
            Action::QueueLast,
            Action::History(1),
            Action::HistoryFirst,
            Action::HistoryLast,
            Action::Refresh,
            Action::Lyrics,
            Action::LyricsPage(2),
            Action::LyricsFirst,
            Action::LyricsLast,
            Action::AutoplayToggle,
            Action::EqOpen,
            Action::EqStep(-3),
            Action::EqStep(1),
            Action::EqFlat,
            Action::EqToggle,
            Action::Select("search".into()),
            Action::Confirm("ab12".into()),
            Action::Cancel,
            Action::Setting("normalize".into()),
            Action::Play("0193a1c2-0000-7000-8000-000000000000".into()),
        ];
        for a in actions {
            let id = CustomId::new(2, 123456789012345678, a.clone());
            let s = id.to_string();
            assert!(s.len() < 100, "{s}");
            assert_eq!(s.parse::<CustomId>().unwrap(), id, "{s}");
        }
        for o in [Origin::Queue(2), Origin::History(0), Origin::Lyrics(7)] {
            let id = CustomId::new(1, 5, Action::Skip).on(Some(o));
            let s = id.to_string();
            assert!(s.ends_with(&o.code()), "{s}");
            assert_eq!(s.parse::<CustomId>().unwrap(), id, "{s}");
        }
        assert_eq!(
            "cd:1:1:5:cl:@q3".parse::<CustomId>().unwrap().origin,
            Some(Origin::Queue(3))
        );
        assert!("cd:1:1:5:sk:@x1".parse::<CustomId>().is_err());
        assert!("cd:1:1:5:sk:@".parse::<CustomId>().is_err());
        assert!("cd:1:1:5:sk:@q".parse::<CustomId>().is_err());
    }

    #[test]
    fn rejects_foreign_and_malformed() {
        assert!("cd:1:0:1:pl".parse::<CustomId>().is_ok());
        assert!("cd:2:0:1:pl".parse::<CustomId>().is_err());
        assert!("cd:1:x:1:pl".parse::<CustomId>().is_err());
        assert!("cd:1:0:1:zz".parse::<CustomId>().is_err());
        assert!("cd:1:0:1:q".parse::<CustomId>().is_err());
        assert!("cd:1:0:1:q:notanumber".parse::<CustomId>().is_err());
        assert!("something-else".parse::<CustomId>().is_err());
    }
}
