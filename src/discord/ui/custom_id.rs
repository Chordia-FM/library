//! Component `custom_id`s: `cd:1:<bot>:<guild>:<action>[:<arg>]`.
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
    /// Open the queue as a new private message (the controller's Queue button).
    QueueOpen,
    /// Show a queue page (0-based) in place.
    Queue(u32),
    /// Jump to the first / last queue page. Separate from `Queue(n)` so the edge buttons never
    /// share a custom id with their neighbours (Discord refuses duplicates in one message).
    QueueFirst,
    QueueLast,
    /// Re-render the now-playing controller.
    Refresh,
    Lyrics,
    AutoplayToggle,
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
            Action::QueueOpen => "qo".into(),
            Action::Queue(p) => format!("q:{p}"),
            Action::QueueFirst => "qf".into(),
            Action::QueueLast => "ql".into(),
            Action::Refresh => "np".into(),
            Action::Lyrics => "ly".into(),
            Action::AutoplayToggle => "ap".into(),
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
            ("qo", None) => Action::QueueOpen,
            ("q", Some(p)) => Action::Queue(p.parse().ok()?),
            ("qf", None) => Action::QueueFirst,
            ("ql", None) => Action::QueueLast,
            ("np", None) => Action::Refresh,
            ("ly", None) => Action::Lyrics,
            ("ap", None) => Action::AutoplayToggle,
            ("sel", Some(c)) => Action::Select(c.to_string()),
            ("cf", Some(n)) => Action::Confirm(n.to_string()),
            ("cx", None) => Action::Cancel,
            ("ss", Some(s)) => Action::Setting(s.to_string()),
            ("py", Some(id)) => Action::Play(id.to_string()),
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
}

impl CustomId {
    pub fn new(bot: u8, guild: u64, action: Action) -> Self {
        Self { bot, guild, action }
    }
}

impl fmt::Display for CustomId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "cd:1:{}:{}:{}", self.bot, self.guild, self.action.code())
    }
}

impl FromStr for CustomId {
    type Err = ();

    fn from_str(s: &str) -> Result<Self, ()> {
        let mut parts = s.splitn(6, ':');
        if parts.next() != Some("cd") || parts.next() != Some("1") {
            return Err(());
        }
        let bot = parts.next().and_then(|b| b.parse().ok()).ok_or(())?;
        let guild = parts.next().and_then(|g| g.parse().ok()).ok_or(())?;
        let code = parts.next().ok_or(())?;
        let arg = parts.next();
        let action = Action::parse(code, arg).ok_or(())?;
        Ok(CustomId { bot, guild, action })
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
            Action::QueueOpen,
            Action::Queue(3),
            Action::QueueFirst,
            Action::QueueLast,
            Action::Refresh,
            Action::Lyrics,
            Action::AutoplayToggle,
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
