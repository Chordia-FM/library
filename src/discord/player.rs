//! Per-guild playback: the queue, what is playing, and the controller message that shows it.
//!
//! One [`GuildPlayer`] per (identity, guild). songbird owns the audio (mixing, Opus, the voice
//! connection); this owns everything a listener would call "the player": queue order, loop mode,
//! history, volume with ReplayGain, who is listening, and when to give up and leave.
//!
//! State lives behind one async mutex and every operation is short: lock, decide, unlock, then do
//! the slow things (decode, HTTP) outside it. Track transitions arrive from songbird's event task as
//! [`TrackEnd`]; each carries the epoch of the track it belongs to, so a late event from a track that
//! was already skipped cannot advance the queue twice.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::{Arc, Weak};
use std::time::{Duration, Instant};

use poise::async_trait;
use serenity::all::{ChannelId, GuildId, MessageId, UserId};
use songbird::driver::Bitrate;
use songbird::events::{Event, EventContext, EventHandler, TrackEvent};
use songbird::tracks::{PlayMode, TrackHandle};
use tokio::sync::{Mutex, Notify};

use chordia_contracts::discord::ResolvedTrack;
use chordia_contracts::scrobble::{ClientType, ListeningEvent, PlaybackSource};
use chordia_contracts::social::NowPlayingReport;
use uuid::Uuid;

use crate::catalog::TrackRow;
use crate::discord::emoji::IconSet;
use crate::discord::identity::Identity;
use crate::discord::presence;
use crate::discord::settings::{self, GuildSettings};
use crate::discord::source::{self, TrackFacts};
use crate::discord::ui::{self, views};
use crate::discord::{autoplay, hub};

/// How many finished tracks `/back` and `/history` can reach.
/// How often a long track re-tells the Hub who is hearing it (its live entry expires in twelve).
const NOW_PLAYING_REFRESH: Duration = Duration::from_secs(5 * 60);

const HISTORY_CAP: usize = 50;
/// How long the controller waits after a change before re-rendering, so a burst of button presses
/// costs one edit.
const CONTROLLER_COALESCE: Duration = Duration::from_millis(1200);
/// How many newer messages may sit below the controller before it is re-posted at the bottom.

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum LoopMode {
    #[default]
    Off,
    Track,
    Queue,
}

impl LoopMode {
    pub fn cycle(self) -> Self {
        match self {
            LoopMode::Off => LoopMode::Track,
            LoopMode::Track => LoopMode::Queue,
            LoopMode::Queue => LoopMode::Off,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            LoopMode::Off => "off",
            LoopMode::Track => "track",
            LoopMode::Queue => "queue",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Position {
    Last,
    Next,
}

#[derive(Debug, Clone)]
pub struct QueueItem {
    pub track: Arc<TrackRow>,
    pub requested_by: UserId,
    /// Picked by the radio when the queue ran out, not asked for by anyone.
    pub autoplay: bool,
}

/// Why the bot left a voice channel — the Left notice says so.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LeaveReason {
    Command,
    Idle,
    Alone,
    Shutdown,
    Disconnected,
}

#[derive(Debug, thiserror::Error)]
pub enum PlayerError {
    #[error("I'm not in a voice channel")]
    NotInVoice,
    #[error("couldn't join the voice channel ({0})")]
    Join(String),
    #[error("nothing is playing")]
    NothingPlaying,
    #[error("couldn't play that ({0})")]
    Source(String),
    #[error("that isn't a position in the queue")]
    BadIndex,
    #[error("this file can't be seeked while it plays")]
    CannotSeek,
    #[error("the bot is not connected to Discord")]
    Offline,
    #[error("nothing to go back to")]
    NoHistory,
}

pub type PlayerResult<T> = Result<T, PlayerError>;

/// A track's cover art as Discord sees it: a file named per track, uploaded with the first message
/// that shows it and then kept by attachment id on every edit of that message.
#[derive(Debug, Clone)]
pub struct Cover {
    pub filename: String,
    pub bytes: Arc<Vec<u8>>,
    /// Set once the controller has uploaded it, so edits can keep it instead of re-sending it.
    pub attachment_id: Option<u64>,
}

/// Art bigger than this is left out rather than uploaded on every track change.
const COVER_MAX_BYTES: usize = 4 * 1024 * 1024;

impl Cover {
    pub async fn load(db: &sqlx::SqlitePool, track: &TrackRow) -> Option<Cover> {
        let (mime, bytes) = crate::catalog::get_track_cover(db, &track.id)
            .await
            .ok()
            .flatten()?;
        if bytes.is_empty() || bytes.len() > COVER_MAX_BYTES {
            return None;
        }
        let ext = match mime.as_str() {
            "image/png" => "png",
            "image/webp" => "webp",
            "image/gif" => "gif",
            _ => "jpg",
        };
        let short: String = track
            .id
            .chars()
            .filter(|c| c.is_alphanumeric())
            .take(8)
            .collect();
        Some(Cover {
            filename: format!("cover-{short}.{ext}"),
            bytes: Arc::new(bytes),
            attachment_id: None,
        })
    }
}

/// The playing track and its live handle.
struct Playing {
    item: QueueItem,
    handle: TrackHandle,
    facts: TrackFacts,
    cover: Option<Cover>,
    paused: bool,
    /// Row id in `discord_plays`, finalised with `ms_played` when the track ends.
    play_id: Option<i64>,
    epoch: u64,
    /// The native (Symphonia) path seeks; the ffmpeg pipe does not.
    seekable: bool,
    /// Wall-clock accounting for `ms_played`: time spent unpaused so far.
    played: Duration,
    resumed_at: Option<Instant>,
    /// Wall clock when it started, for the listening events.
    started_wall: i64,
    /// Unpaused milliseconds each listener was present for, settled whenever who is present
    /// changes and when the track ends.
    heard: HashMap<UserId, u64>,
    settled_at: Instant,
    /// Set once the play's listening events were queued: a stop and the end event that follows
    /// it must not both report.
    reported: bool,
    /// The Hub's ids for the track, once the lookup answered.
    links: Option<ResolvedTrack>,
}

/// What is left of a play once it ended: what the local log needs, and who heard how much.
struct Concluded {
    play_id: Option<i64>,
    ms_played: u64,
    heard: Vec<(UserId, u64)>,
    track: Arc<TrackRow>,
    started_wall: i64,
}

impl Playing {
    fn ms_played(&self) -> u64 {
        let live = self.resumed_at.map(|t| t.elapsed()).unwrap_or_default();
        (self.played + live).as_millis() as u64
    }

    /// Credit everyone `present` with the unpaused time since the last settle.
    fn settle(&mut self, present: &HashSet<UserId>) {
        let now = Instant::now();
        if let Some(resumed) = self.resumed_at {
            let from = if resumed > self.settled_at {
                resumed
            } else {
                self.settled_at
            };
            let ms = now.saturating_duration_since(from).as_millis() as u64;
            if ms > 0 {
                for user in present {
                    *self.heard.entry(*user).or_insert(0) += ms;
                }
            }
        }
        self.settled_at = now;
    }

    /// Close the books on this play, once.
    fn conclude(&mut self, present: &HashSet<UserId>) -> Option<Concluded> {
        if self.reported {
            return None;
        }
        self.settle(present);
        self.reported = true;
        Some(Concluded {
            play_id: self.play_id,
            ms_played: self.ms_played(),
            heard: self.heard.iter().map(|(u, ms)| (*u, *ms)).collect(),
            track: self.item.track.clone(),
            started_wall: self.started_wall,
        })
    }
}

struct PlayerState {
    voice_channel: Option<ChannelId>,
    text_channel: Option<ChannelId>,
    queue: VecDeque<QueueItem>,
    history: VecDeque<QueueItem>,
    current: Option<Playing>,
    loop_mode: LoopMode,
    autoplay: bool,
    /// Play the queue in random order: each advance takes a random item rather than the head.
    shuffle: bool,
    volume: u8,
    normalize: bool,
    /// Non-bot users in the bot's voice channel.
    listeners: HashSet<UserId>,
    /// Since when nothing has been playing.
    idle_since: Option<Instant>,
    /// Since when the bot has been playing to nobody.
    alone_since: Option<Instant>,
    controller: Option<(ChannelId, MessageId)>,
    messages_since_controller: u32,
    bitrate_kbps: Option<u32>,
    epoch: u64,
    /// The track to play next regardless of the queue (`/back`, `/jump`).
    next_override: Option<QueueItem>,
    /// `/skip` on a looping track: advance instead of repeating.
    skip_requested: bool,
    /// `/stop` and leave: the End event must not start the next track.
    stopping: bool,
    settings: GuildSettings,
    /// The Chordia users last told they are "listening now", to tell them it stopped.
    now_playing_for: Vec<Uuid>,
    now_playing_at: Option<Instant>,
}

/// Everything a view needs, cloned out from under the lock.
#[derive(Debug, Clone)]
pub struct CurrentSnapshot {
    pub item: QueueItem,
    pub facts: TrackFacts,
    pub position_ms: u64,
    pub paused: bool,
    pub cover: Option<Cover>,
    /// The Hub's ids for the track, for deep links; absent until looked up, or without a Hub.
    pub links: Option<ResolvedTrack>,
}

#[derive(Debug, Clone)]
pub struct PlayerSnapshot {
    pub bot_index: u8,
    pub bot_name: String,
    pub icons: Arc<IconSet>,
    /// The web client's origin when the library is paired to a Hub; views link into it.
    pub web_base: Option<String>,
    pub guild_id: GuildId,
    pub voice_channel: Option<ChannelId>,
    pub voice_channel_name: Option<String>,
    pub current: Option<CurrentSnapshot>,
    pub queue: Vec<QueueItem>,
    pub history: Vec<QueueItem>,
    pub loop_mode: LoopMode,
    pub autoplay: bool,
    pub shuffle: bool,
    pub volume: u8,
    pub normalize: bool,
    pub listeners: usize,
}

impl PlayerSnapshot {
    pub fn queue_duration_ms(&self) -> u64 {
        self.queue
            .iter()
            .map(|i| i.track.duration_ms.max(0) as u64)
            .sum()
    }

    /// Milliseconds until the item at `index` (0-based in the queue) starts, given what is playing
    /// and what is ahead of it.
    pub fn eta_ms(&self, index: usize) -> u64 {
        let remaining = self
            .current
            .as_ref()
            .map(|c| (c.item.track.duration_ms.max(0) as u64).saturating_sub(c.position_ms))
            .unwrap_or(0);
        remaining
            + self
                .queue
                .iter()
                .take(index)
                .map(|i| i.track.duration_ms.max(0) as u64)
                .sum::<u64>()
    }
}

/// What `enqueue` did, for the confirmation view.
#[derive(Debug, Clone)]
pub struct Enqueued {
    /// 0 = started playing immediately; n = it is the n-th item in the queue.
    pub position: usize,
    pub count: usize,
}

pub struct GuildPlayer {
    pub guild_id: GuildId,
    identity: Weak<Identity>,
    inner: Mutex<PlayerState>,
    controller_wake: Notify,
}

impl GuildPlayer {
    pub fn new(
        identity: &Arc<Identity>,
        guild_id: GuildId,
        settings: GuildSettings,
        default_volume: u8,
    ) -> Arc<Self> {
        let player = Arc::new(Self {
            guild_id,
            identity: Arc::downgrade(identity),
            inner: Mutex::new(PlayerState {
                voice_channel: None,
                text_channel: None,
                queue: VecDeque::new(),
                history: VecDeque::new(),
                current: None,
                loop_mode: LoopMode::Off,
                autoplay: settings.autoplay,
                shuffle: false,
                volume: settings.volume.unwrap_or(default_volume),
                normalize: settings.normalize,
                listeners: HashSet::new(),
                idle_since: None,
                alone_since: None,
                controller: None,
                messages_since_controller: 0,
                now_playing_for: Vec::new(),
                now_playing_at: None,
                bitrate_kbps: None,
                epoch: 0,
                next_override: None,
                skip_requested: false,
                stopping: false,
                settings,
            }),
            controller_wake: Notify::new(),
        });
        player.spawn_controller_task();
        player
    }

    fn identity(&self) -> PlayerResult<Arc<Identity>> {
        self.identity.upgrade().ok_or(PlayerError::Offline)
    }

    // ---- connection -------------------------------------------------------------------------

    pub async fn voice_channel(&self) -> Option<ChannelId> {
        self.inner.lock().await.voice_channel
    }

    pub async fn text_channel(&self) -> Option<ChannelId> {
        self.inner.lock().await.text_channel
    }

    pub async fn is_playing(&self) -> bool {
        self.inner.lock().await.current.is_some()
    }

    pub async fn listener_count(&self) -> usize {
        self.inner.lock().await.listeners.len()
    }

    pub async fn has_listeners(&self) -> bool {
        !self.inner.lock().await.listeners.is_empty()
    }

    /// Nobody but (at most) `user` is listening — the case where DJ rules do not apply.
    pub async fn is_alone_with(&self, user: UserId) -> bool {
        let s = self.inner.lock().await;
        s.listeners.is_empty() || (s.listeners.len() == 1 && s.listeners.contains(&user))
    }

    /// The gateway says the bot was moved to another voice channel.
    pub async fn moved_to(&self, channel: ChannelId) {
        let mut s = self.inner.lock().await;
        if s.voice_channel.is_some() && s.voice_channel != Some(channel) {
            s.voice_channel = Some(channel);
        }
    }

    /// Join (or move to) a voice channel, announcing in `text`.
    pub async fn join(&self, voice: ChannelId, text: ChannelId) -> PlayerResult<()> {
        let identity = self.identity()?;
        let songbird = identity.songbird().ok_or(PlayerError::Offline)?;
        let already_there = self.inner.lock().await.voice_channel == Some(voice);
        let bitrate_kbps = identity.channel_bitrate_kbps(self.guild_id, voice);
        if !already_there {
            let call = songbird
                .join(self.guild_id, voice)
                .await
                .map_err(|e| PlayerError::Join(e.to_string()))?;
            let mut call = call.lock().await;
            // Nobody hears through a bot; not decoding incoming audio saves the CPU for encoding.
            let _ = call.deafen(true).await;
            if let Some(kbps) = bitrate_kbps {
                call.set_bitrate(Bitrate::Bits((kbps * 1000) as i32));
            }
        }
        let mut s = self.inner.lock().await;
        s.voice_channel = Some(voice);
        s.text_channel = Some(text);
        s.bitrate_kbps = bitrate_kbps;
        if s.current.is_none() && s.idle_since.is_none() {
            s.idle_since = Some(Instant::now());
        }
        drop(s);
        identity.refresh_listeners(self).await;
        Ok(())
    }

    /// Leave the voice channel, stop everything, and say why.
    pub async fn leave(self: &Arc<Self>, reason: LeaveReason) {
        let (had_channel, controller, text) = {
            let mut s = self.inner.lock().await;
            s.stopping = true;
            s.queue.clear();
            s.history.clear();
            s.next_override = None;
            if let Some(cur) = &s.current {
                let _ = cur.handle.stop();
            }
            (s.voice_channel.take(), s.controller.take(), s.text_channel)
        };
        if let Ok(identity) = self.identity() {
            if let Some(sb) = identity.songbird() {
                let _ = sb.remove(self.guild_id).await;
            }
            if let Some(vc) = had_channel {
                presence::clear_voice_status(&identity, vc).await;
            }
            self.finish_current_play().await;
            {
                let mut s = self.inner.lock().await;
                s.current = None;
                s.stopping = false;
                s.idle_since = None;
                s.alone_since = None;
                s.listeners.clear();
            }
            self.push_now_playing().await;
            presence::update(&identity).await;
            if had_channel.is_some() {
                let snap = self.snapshot().await;
                let msg = views::left(&snap, reason);
                if let Some(http) = identity.http() {
                    match controller {
                        Some((ch, id)) => {
                            let _ = ui::send::edit(&http, ch, id, msg).await;
                        }
                        None => {
                            if let Some(ch) = text {
                                let _ = ui::send::post(&http, ch, msg).await;
                            }
                        }
                    }
                }
            }
        }
    }

    /// The gateway says the bot is no longer in a voice channel (kicked, channel deleted).
    pub async fn on_disconnected(self: &Arc<Self>) {
        if self.inner.lock().await.voice_channel.is_some() {
            self.leave(LeaveReason::Disconnected).await;
        }
    }

    // ---- queue ------------------------------------------------------------------------------

    /// Add tracks and start playing if idle.
    pub async fn enqueue(
        self: &Arc<Self>,
        items: Vec<QueueItem>,
        at: Position,
    ) -> PlayerResult<Enqueued> {
        if items.is_empty() {
            return Ok(Enqueued {
                position: 0,
                count: 0,
            });
        }
        let count = items.len();
        let (start_now, position) = {
            let mut s = self.inner.lock().await;
            if s.voice_channel.is_none() {
                return Err(PlayerError::NotInVoice);
            }
            match at {
                Position::Next => {
                    for item in items.into_iter().rev() {
                        s.queue.push_front(item);
                    }
                }
                Position::Last => s.queue.extend(items),
            }
            let position = match at {
                Position::Next => 1,
                Position::Last => s.queue.len() - count + 1,
            };
            (s.current.is_none(), position)
        };
        if start_now {
            self.advance().await;
            Ok(Enqueued { position: 0, count })
        } else {
            self.controller_wake.notify_one();
            Ok(Enqueued { position, count })
        }
    }

    pub async fn skip(&self) -> PlayerResult<QueueItem> {
        let mut s = self.inner.lock().await;
        let cur = s.current.as_ref().ok_or(PlayerError::NothingPlaying)?;
        let item = cur.item.clone();
        let handle = cur.handle.clone();
        s.skip_requested = true;
        let _ = handle.stop();
        Ok(item)
    }

    /// Go back to the previous track; the current one returns to the front of the queue.
    pub async fn previous(&self) -> PlayerResult<QueueItem> {
        let mut s = self.inner.lock().await;
        let prev = s.history.pop_back().ok_or(PlayerError::NoHistory)?;
        s.next_override = Some(prev.clone());
        match s
            .current
            .as_ref()
            .map(|c| (c.item.clone(), c.handle.clone()))
        {
            Some((item, handle)) => {
                s.queue.push_front(item);
                s.skip_requested = true;
                let _ = handle.stop();
            }
            None => {
                drop(s);
                // Nothing to end, so nothing will call advance for us.
                return self.start_override().await.map(|_| prev);
            }
        }
        Ok(prev)
    }

    async fn start_override(&self) -> PlayerResult<()> {
        let item = self.inner.lock().await.next_override.take();
        match item {
            Some(item) => self.start(item).await,
            None => Ok(()),
        }
    }

    pub async fn pause(&self) -> PlayerResult<()> {
        let mut s = self.inner.lock().await;
        let cur = s.current.as_mut().ok_or(PlayerError::NothingPlaying)?;
        if !cur.paused {
            let _ = cur.handle.pause();
            cur.paused = true;
            if let Some(t) = cur.resumed_at.take() {
                cur.played += t.elapsed();
            }
        }
        drop(s);
        self.after_change().await;
        Ok(())
    }

    pub async fn resume(&self) -> PlayerResult<()> {
        let mut s = self.inner.lock().await;
        let cur = s.current.as_mut().ok_or(PlayerError::NothingPlaying)?;
        if cur.paused {
            let _ = cur.handle.play();
            cur.paused = false;
            cur.resumed_at = Some(Instant::now());
        }
        drop(s);
        self.after_change().await;
        Ok(())
    }

    /// Toggle pause; returns `true` when now paused.
    pub async fn toggle_pause(&self) -> PlayerResult<bool> {
        let paused = self
            .inner
            .lock()
            .await
            .current
            .as_ref()
            .ok_or(PlayerError::NothingPlaying)?
            .paused;
        if paused {
            self.resume().await?;
        } else {
            self.pause().await?;
        }
        Ok(!paused)
    }

    /// Stop playback and clear the queue, staying in the channel.
    pub async fn stop(self: &Arc<Self>) -> PlayerResult<()> {
        let handle = {
            let mut s = self.inner.lock().await;
            s.queue.clear();
            // A stop ends the session: what played before it is not somewhere `/back` should go.
            s.history.clear();
            s.next_override = None;
            let handle = s
                .current
                .as_ref()
                .ok_or(PlayerError::NothingPlaying)?
                .handle
                .clone();
            s.stopping = true;
            handle
        };
        let _ = handle.stop();
        Ok(())
    }

    pub async fn seek(&self, to: Duration) -> PlayerResult<Duration> {
        let (handle, seekable) = {
            let s = self.inner.lock().await;
            let cur = s.current.as_ref().ok_or(PlayerError::NothingPlaying)?;
            (cur.handle.clone(), cur.seekable)
        };
        if !seekable {
            return Err(PlayerError::CannotSeek);
        }
        let got = handle
            .seek_async(to)
            .await
            .map_err(|e| PlayerError::Source(e.to_string()))?;
        self.after_change().await;
        Ok(got)
    }

    pub async fn position(&self) -> Option<Duration> {
        let handle = self.inner.lock().await.current.as_ref()?.handle.clone();
        handle.get_info().await.ok().map(|i| i.position)
    }

    pub async fn set_volume(&self, pct: u8) -> PlayerResult<u8> {
        let pct = pct.min(150);
        let mut s = self.inner.lock().await;
        s.volume = pct;
        s.settings.volume = Some(pct);
        if let Some(cur) = &s.current {
            let v = effective_volume(&s, &cur.facts);
            let _ = cur.handle.set_volume(v);
        }
        let settings = s.settings.clone();
        drop(s);
        self.persist(settings).await;
        self.after_change().await;
        Ok(pct)
    }

    pub async fn volume(&self) -> u8 {
        self.inner.lock().await.volume
    }

    pub async fn set_loop(&self, mode: LoopMode) -> LoopMode {
        self.inner.lock().await.loop_mode = mode;
        self.after_change().await;
        mode
    }

    pub async fn cycle_loop(&self) -> LoopMode {
        let mode = {
            let mut s = self.inner.lock().await;
            s.loop_mode = s.loop_mode.cycle();
            s.loop_mode
        };
        self.after_change().await;
        mode
    }

    pub async fn set_autoplay(&self, on: bool) -> bool {
        let settings = {
            let mut s = self.inner.lock().await;
            s.autoplay = on;
            s.settings.autoplay = on;
            s.settings.clone()
        };
        self.persist(settings).await;
        self.after_change().await;
        on
    }

    pub async fn set_normalize(&self, on: bool) -> bool {
        let settings = {
            let mut s = self.inner.lock().await;
            s.normalize = on;
            s.settings.normalize = on;
            if let Some(cur) = &s.current {
                let v = effective_volume(&s, &cur.facts);
                let _ = cur.handle.set_volume(v);
            }
            s.settings.clone()
        };
        self.persist(settings).await;
        on
    }

    /// Toggle random order; returns `true` when now on.
    pub async fn toggle_shuffle(&self) -> bool {
        let on = {
            let mut s = self.inner.lock().await;
            s.shuffle = !s.shuffle;
            s.shuffle
        };
        self.after_change().await;
        on
    }

    /// Remove the item at `index` (1-based, as shown in `/queue`).
    pub async fn remove(&self, index: usize) -> PlayerResult<QueueItem> {
        let item = {
            let mut s = self.inner.lock().await;
            if index == 0 || index > s.queue.len() {
                return Err(PlayerError::BadIndex);
            }
            s.queue.remove(index - 1).ok_or(PlayerError::BadIndex)?
        };
        self.after_change().await;
        Ok(item)
    }

    /// Move the item at `from` to `to` (both 1-based).
    pub async fn move_item(&self, from: usize, to: usize) -> PlayerResult<QueueItem> {
        let item = {
            let mut s = self.inner.lock().await;
            let n = s.queue.len();
            if from == 0 || to == 0 || from > n || to > n {
                return Err(PlayerError::BadIndex);
            }
            let item = s.queue.remove(from - 1).ok_or(PlayerError::BadIndex)?;
            s.queue.insert(to - 1, item.clone());
            item
        };
        self.after_change().await;
        Ok(item)
    }

    /// Skip straight to the item at `index` (1-based); everything before it is dropped.
    pub async fn jump(&self, index: usize) -> PlayerResult<QueueItem> {
        let mut s = self.inner.lock().await;
        if index == 0 || index > s.queue.len() {
            return Err(PlayerError::BadIndex);
        }
        s.queue.drain(..index - 1);
        let item = s.queue.pop_front().ok_or(PlayerError::BadIndex)?;
        s.next_override = Some(item.clone());
        match s.current.as_ref().map(|c| c.handle.clone()) {
            Some(handle) => {
                s.skip_requested = true;
                let _ = handle.stop();
                Ok(item)
            }
            None => {
                drop(s);
                self.start_override().await.map(|_| item)
            }
        }
    }

    pub async fn clear(&self) -> usize {
        let n = {
            let mut s = self.inner.lock().await;
            let n = s.queue.len();
            s.queue.clear();
            n
        };
        self.after_change().await;
        n
    }

    pub async fn snapshot(&self) -> PlayerSnapshot {
        let identity = self.identity.upgrade();
        let web_base = match &identity {
            Some(i) => i.web_base().await,
            None => None,
        };
        let (handle, mut snap) = {
            let s = self.inner.lock().await;
            let (bot_index, bot_name) = match &identity {
                Some(i) => (i.index, i.display_name_sync()),
                None => (0, "Chordia".to_string()),
            };
            let handle = s.current.as_ref().map(|c| c.handle.clone());
            let icons = identity.as_ref().map(|i| i.icons()).unwrap_or_default();
            let voice_channel_name = match (&identity, s.voice_channel) {
                (Some(i), Some(vc)) => i.channel_name(self.guild_id, vc),
                _ => None,
            };
            let snap = PlayerSnapshot {
                bot_index,
                bot_name,
                icons,
                web_base,
                guild_id: self.guild_id,
                voice_channel: s.voice_channel,
                voice_channel_name,
                current: s.current.as_ref().map(|c| CurrentSnapshot {
                    item: c.item.clone(),
                    facts: c.facts.clone(),
                    position_ms: 0,
                    paused: c.paused,
                    cover: c.cover.clone(),
                    links: c.links.clone(),
                }),
                queue: s.queue.iter().cloned().collect(),
                history: s.history.iter().cloned().collect(),
                loop_mode: s.loop_mode,
                autoplay: s.autoplay,
                shuffle: s.shuffle,
                volume: s.volume,
                normalize: s.normalize,
                listeners: s.listeners.len(),
            };
            (handle, snap)
        };
        if let (Some(h), Some(cur)) = (handle, snap.current.as_mut()) {
            if let Ok(info) = h.get_info().await {
                cur.position_ms = info.position.as_millis() as u64;
            }
        }
        snap
    }

    pub async fn settings(&self) -> GuildSettings {
        self.inner.lock().await.settings.clone()
    }

    pub async fn update_settings(&self, f: impl FnOnce(&mut GuildSettings)) -> GuildSettings {
        let settings = {
            let mut s = self.inner.lock().await;
            f(&mut s.settings);
            s.autoplay = s.settings.autoplay;
            s.normalize = s.settings.normalize;
            if let Some(v) = s.settings.volume {
                s.volume = v;
            }
            s.settings.clone()
        };
        self.persist(settings.clone()).await;
        settings
    }

    // ---- listeners & idle ----------------------------------------------------------------------

    /// The gateway's view of who is in the bot's channel, minus bots.
    pub async fn set_listeners(&self, users: HashSet<UserId>) {
        let ids: Vec<u64> = {
            let mut s = self.inner.lock().await;
            let was_alone = s.listeners.is_empty();
            // Whoever was here gets credited up to now before the set changes.
            let present = s.listeners.clone();
            if let Some(cur) = s.current.as_mut() {
                cur.settle(&present);
            }
            s.listeners = users;
            if s.listeners.is_empty() {
                if !was_alone || s.alone_since.is_none() {
                    s.alone_since = Some(Instant::now());
                }
            } else {
                s.alone_since = None;
            }
            s.listeners.iter().map(|u| u.get()).collect()
        };
        // Who among them is a Chordia user: asked once, remembered, shown on the controller, and
        // told what they are hearing.
        let me = self
            .identity
            .upgrade()
            .and_then(|i| i.player_arc(self.guild_id));
        if let (Ok(identity), Some(me)) = (self.identity(), me) {
            let state = identity.state.clone();
            tokio::spawn(async move {
                hub::resolve_listeners(&state, &ids).await;
                me.push_now_playing().await;
            });
        }
    }

    /// Periodic housekeeping: leave when idle or alone for longer than the identity allows, and
    /// keep the controller's progress line moving.
    pub async fn tick(self: &Arc<Self>, idle_timeout: Duration) {
        let (reason, playing, refresh_now_playing) = {
            let s = self.inner.lock().await;
            if s.voice_channel.is_none() {
                return;
            }
            let playing = s.current.is_some();
            // The Hub's live entry expires; keep it fresh through a long track.
            let refresh_now_playing = playing
                && s.now_playing_at
                    .is_none_or(|t| t.elapsed() >= NOW_PLAYING_REFRESH);
            let reason = if s.settings.always_on {
                None
            } else if !playing && s.idle_since.is_some_and(|t| t.elapsed() >= idle_timeout) {
                Some(LeaveReason::Idle)
            } else if s.alone_since.is_some_and(|t| t.elapsed() >= idle_timeout) {
                Some(LeaveReason::Alone)
            } else {
                None
            };
            (reason, playing, refresh_now_playing)
        };
        match reason {
            Some(r) => self.leave(r).await,
            None if playing => {
                self.controller_wake.notify_one();
                if refresh_now_playing {
                    let p = self.clone();
                    tokio::spawn(async move { p.push_now_playing().await });
                }
            }
            None => {}
        }
    }

    /// Tell the Hub what the listeners who are Chordia users are hearing, so their profiles show
    /// it; or, when nothing plays any more, that they stopped. Listeners the cache does not know
    /// yet are asked for here, which is why this runs off the hot path.
    pub async fn push_now_playing(&self) {
        let Ok(identity) = self.identity() else {
            return;
        };
        let (report, ids, previous) = {
            let s = self.inner.lock().await;
            let report = s.current.as_ref().map(|c| {
                let channel = s
                    .voice_channel
                    .and_then(|vc| identity.channel_name(self.guild_id, vc))
                    .unwrap_or_default();
                NowPlayingReport {
                    track_id: c.links.as_ref().map(|l| l.track_id),
                    title: c.item.track.title.clone(),
                    artist: c.item.track.artist.clone(),
                    album: c.item.track.album.clone(),
                    image_url: None,
                    device_id: Some(format!(
                        "discord:{}:{}",
                        identity.app_id_sync().unwrap_or(0),
                        self.guild_id.get()
                    )),
                    device_label: Some(format!("Discord · #{channel}")),
                }
            });
            let ids: Vec<u64> = s.listeners.iter().map(|u| u.get()).collect();
            (report, ids, s.now_playing_for.clone())
        };
        let users: Vec<Uuid> = match &report {
            Some(_) => hub::resolve_listeners(&identity.state, &ids)
                .await
                .into_values()
                .map(|l| l.user_id)
                .collect(),
            None => Vec::new(),
        };
        // Whoever was told last time and is not in this report has stopped hearing it.
        let gone: Vec<Uuid> = previous
            .iter()
            .copied()
            .filter(|u| !users.contains(u))
            .collect();
        {
            let mut s = self.inner.lock().await;
            s.now_playing_for = users.clone();
            s.now_playing_at = Some(Instant::now());
        }
        hub::now_playing(&identity.state, gone, None).await;
        if let Some(report) = report {
            hub::now_playing(&identity.state, users, Some(report)).await;
        }
    }

    /// The gateway reported a channel edit. If it is the bot's channel and the bitrate moved, the
    /// encoder and the badge follow immediately rather than at the next track.
    pub async fn refresh_bitrate(&self, channel: ChannelId, kbps: Option<u32>) {
        let Some(kbps) = kbps else { return };
        {
            let mut s = self.inner.lock().await;
            if s.voice_channel != Some(channel) || s.bitrate_kbps == Some(kbps) {
                return;
            }
            s.bitrate_kbps = Some(kbps);
            if let Some(cur) = s.current.as_mut() {
                cur.facts.opus_kbps = Some(kbps);
            }
        }
        if let Some(call) = self
            .identity()
            .ok()
            .and_then(|i| i.songbird())
            .and_then(|sb| sb.get(self.guild_id))
        {
            call.lock()
                .await
                .set_bitrate(Bitrate::Bits((kbps * 1000) as i32));
        }
        self.controller_wake.notify_one();
    }

    /// A message landed in the controller's channel; past a threshold the controller re-posts.
    pub async fn note_channel_message(&self, channel: ChannelId) {
        let mut s = self.inner.lock().await;
        if s.controller.is_some_and(|(ch, _)| ch == channel) {
            s.messages_since_controller += 1;
        }
    }

    // ---- playback internals --------------------------------------------------------------------

    /// The next queued item: the head, or any item when shuffle is on.
    fn take_next(s: &mut PlayerState) -> Option<QueueItem> {
        if s.shuffle && s.queue.len() > 1 {
            use rand::Rng;
            let i = rand::thread_rng().gen_range(0..s.queue.len());
            s.queue.remove(i)
        } else {
            s.queue.pop_front()
        }
    }

    /// Start the next thing: an override, the looping track, the queue head, or nothing.
    async fn advance(self: &Arc<Self>) {
        let next = {
            let mut s = self.inner.lock().await;
            if let Some(o) = s.next_override.take() {
                Some(o)
            } else {
                Self::take_next(&mut s)
            }
        };
        match next {
            Some(item) => {
                if let Err(e) = self.start(item.clone()).await {
                    tracing::warn!(guild = %self.guild_id, track = %item.track.id, error = %e, "track failed to start; skipping it");
                    self.announce_error(&item, &e).await;
                    // Try the next one rather than stalling the queue on one bad file.
                    Box::pin(self.advance()).await;
                }
            }
            None => self.became_idle().await,
        }
    }

    async fn start(&self, item: QueueItem) -> PlayerResult<()> {
        let identity = self.identity()?;
        let songbird = identity.songbird().ok_or(PlayerError::Offline)?;
        let call = songbird.get(self.guild_id).ok_or(PlayerError::NotInVoice)?;
        let (input, mut facts) = source::input_for(&identity.state, &item.track, None)
            .await
            .map_err(|e| PlayerError::Source(e.to_string()))?;
        let seekable = !matches!(input, songbird::input::Input::Live(..));
        let cover = Cover::load(&identity.state.db, &item.track).await;
        // The channel's bitrate can be changed while the bot sits in it; every track starts at the
        // current value so the encoder (and the badge) follow it.
        let voice = self.inner.lock().await.voice_channel;
        let kbps = voice.and_then(|vc| identity.channel_bitrate_kbps(self.guild_id, vc));
        let handle = {
            let mut call = call.lock().await;
            if let Some(k) = kbps {
                call.set_bitrate(Bitrate::Bits((k * 1000) as i32));
            }
            call.play_only_input(input)
        };
        let self_arc = self
            .identity
            .upgrade()
            .and_then(|i| i.player_arc(self.guild_id));
        let (epoch, listeners, guild_id, app_id) = {
            let mut s = self.inner.lock().await;
            s.epoch += 1;
            if kbps.is_some() {
                s.bitrate_kbps = kbps;
            }
            facts.opus_kbps = s.bitrate_kbps;
            let volume = effective_volume(&s, &facts);
            let _ = handle.set_volume(volume);
            if let Some(p) = &self_arc {
                let _ = handle.add_event(
                    Event::Track(TrackEvent::End),
                    TrackEnd {
                        player: Arc::downgrade(p),
                        epoch: s.epoch,
                    },
                );
                let _ = handle.add_event(
                    Event::Track(TrackEvent::Error),
                    TrackEnd {
                        player: Arc::downgrade(p),
                        epoch: s.epoch,
                    },
                );
            }
            s.current = Some(Playing {
                item: item.clone(),
                handle,
                facts,
                cover,
                paused: false,
                play_id: None,
                epoch: s.epoch,
                seekable,
                played: Duration::ZERO,
                resumed_at: Some(Instant::now()),
                started_wall: settings::now_ms(),
                heard: HashMap::new(),
                settled_at: Instant::now(),
                reported: false,
                links: None,
            });
            s.idle_since = None;
            s.alone_since = if s.listeners.is_empty() {
                Some(Instant::now())
            } else {
                None
            };
            (
                s.epoch,
                s.listeners.len() as u32,
                self.guild_id,
                identity.app_id_sync(),
            )
        };
        if let Some(app_id) = app_id {
            let play_id = settings::record_play(
                &identity.state.db,
                &app_id.to_string(),
                &guild_id.to_string(),
                &item.track.id,
                Some(item.requested_by.get()),
                listeners,
            )
            .await
            .ok();
            let mut s = self.inner.lock().await;
            if let Some(cur) = s.current.as_mut().filter(|c| c.epoch == epoch) {
                cur.play_id = play_id;
            }
        }
        // Where the track's page is on the Hub, for the controller's links. Off the hot path:
        // the first render goes out without them and the next edit carries them.
        if let Some(p) = &self_arc {
            let p = p.clone();
            let state = identity.state.clone();
            let track = item.track.clone();
            tokio::spawn(async move {
                let Some(links) = hub::resolve_track(&state, &track).await else {
                    return;
                };
                let mut s = p.inner.lock().await;
                if let Some(cur) = s.current.as_mut().filter(|c| c.epoch == epoch) {
                    cur.links = Some(links);
                    drop(s);
                    p.controller_wake.notify_one();
                }
            });
        }
        if let Some(p) = &self_arc {
            let p = p.clone();
            tokio::spawn(async move { p.push_now_playing().await });
        }
        self.controller_wake.notify_one();
        presence::update(&identity).await;
        if let Some(vc) = self.inner.lock().await.voice_channel {
            presence::set_voice_status(&identity, vc, Some(&item.track)).await;
        }
        Ok(())
    }

    /// The radio's pick when the queue ran out and autoplay is on here and allowed: nearest to
    /// `seed` (else the last thing that played), skipping recent history.
    async fn autoplay_pick(&self, seed: Option<&TrackRow>) -> Option<QueueItem> {
        let (on, exclude, last) = {
            let s = self.inner.lock().await;
            let on = s.autoplay && s.settings.can_autoplay && s.voice_channel.is_some();
            let exclude: Vec<String> = s.history.iter().map(|i| i.track.id.clone()).collect();
            (on, exclude, s.history.back().map(|i| i.track.clone()))
        };
        if !on {
            return None;
        }
        let identity = self.identity().ok()?;
        let seed_owned;
        let seed: &TrackRow = match seed {
            Some(t) => t,
            None => {
                seed_owned = last?;
                &seed_owned
            }
        };
        let track = autoplay::pick(&identity.state.db, seed, &exclude).await?;
        Some(QueueItem {
            track: Arc::new(track),
            requested_by: identity.user_id().unwrap_or(UserId::new(1)),
            autoplay: true,
        })
    }

    /// Finish the local play log and queue a listening event for everyone who heard enough of
    /// the track: thirty seconds, or half of a shorter one. Which of them the Hub counts for is
    /// the Hub's decision when the reporter sends.
    async fn report(&self, concluded: Option<Concluded>) {
        let Some(c) = concluded else { return };
        let Ok(identity) = self.identity() else {
            return;
        };
        let db = &identity.state.db;
        if let Some(id) = c.play_id {
            let _ = settings::finish_play(db, id, c.ms_played).await;
        }
        let duration = c.track.duration_ms.max(0) as u64;
        let min = (duration / 2).clamp(1_000, 30_000);
        let heard: Vec<(UserId, u64)> = c.heard.into_iter().filter(|(_, ms)| *ms >= min).collect();
        if heard.is_empty() {
            return;
        }
        let counted = heard.len();
        let library_id = hub::hub_library_id(&identity.state, &c.track.library_id).await;
        let fingerprint = (*c.track).clone().into_contract().fingerprint;
        for (user, ms) in heard {
            let event = ListeningEvent {
                event_id: Uuid::now_v7(),
                fingerprint: fingerprint.clone(),
                title: Some(c.track.title.clone()),
                artist: Some(c.track.artist.clone()),
                started_at: c.started_wall,
                ms_played: ms.min(u32::MAX as u64) as u32,
                duration_ms: duration.min(u32::MAX as u64) as u32,
                source: PlaybackSource::OwnLibrary,
                client_type: ClientType::Discord,
                library_id,
                room_id: None,
                playlist_id: None,
            };
            if let Err(e) = crate::scrobble::enqueue_attributed(db, &event, user.get()).await {
                tracing::warn!(error = %e, guild = %self.guild_id, "queueing a listener's play");
            }
        }
        if let Some(id) = c.play_id {
            let _ = settings::mark_scrobbled(db, id, counted).await;
        }
    }

    /// Called from the songbird event task when the track with `epoch` ended or errored.
    async fn on_track_end(self: &Arc<Self>, epoch: u64, errored: bool) {
        let ended = {
            let mut s = self.inner.lock().await;
            match s.current.as_ref() {
                Some(cur) if cur.epoch == epoch => s.current.take(),
                _ => None,
            }
        };
        let Some(mut ended) = ended else { return };
        if errored {
            tracing::warn!(guild = %self.guild_id, track = %ended.item.track.id, "track errored during playback");
        }
        let present = self.inner.lock().await.listeners.clone();
        self.report(ended.conclude(&present)).await;
        let mut stopped = false;
        let next = {
            let mut s = self.inner.lock().await;
            if s.stopping {
                s.stopping = false;
                s.skip_requested = false;
                stopped = true;
                None
            } else {
                let repeat = s.loop_mode == LoopMode::Track
                    && !s.skip_requested
                    && s.next_override.is_none();
                s.skip_requested = false;
                if !repeat {
                    s.history.push_back(ended.item.clone());
                    while s.history.len() > HISTORY_CAP {
                        s.history.pop_front();
                    }
                }
                if repeat {
                    Some(ended.item.clone())
                } else if let Some(o) = s.next_override.take() {
                    Some(o)
                } else {
                    if s.loop_mode == LoopMode::Queue {
                        s.queue.push_back(ended.item.clone());
                    }
                    Self::take_next(&mut s)
                }
            }
        };
        let next = match next {
            None if !stopped => self.autoplay_pick(Some(&ended.item.track)).await,
            other => other,
        };
        match next {
            Some(item) => {
                if let Err(e) = self.start(item.clone()).await {
                    tracing::warn!(guild = %self.guild_id, track = %item.track.id, error = %e, "next track failed to start");
                    self.announce_error(&item, &e).await;
                    self.advance().await;
                }
            }
            None => self.became_idle().await,
        }
    }

    async fn became_idle(self: &Arc<Self>) {
        let vc = {
            let mut s = self.inner.lock().await;
            s.idle_since = Some(Instant::now());
            s.voice_channel
        };
        self.controller_wake.notify_one();
        // Nothing plays: the listeners' profiles say so.
        self.push_now_playing().await;
        if let Ok(identity) = self.identity() {
            presence::update(&identity).await;
            if let Some(vc) = vc {
                presence::clear_voice_status(&identity, vc).await;
            }
        }
    }

    async fn finish_current_play(&self) {
        let concluded = {
            let mut s = self.inner.lock().await;
            let present = s.listeners.clone();
            s.current.as_mut().and_then(|c| c.conclude(&present))
        };
        self.report(concluded).await;
    }

    async fn announce_error(&self, item: &QueueItem, err: &PlayerError) {
        let Ok(identity) = self.identity() else {
            return;
        };
        let Some(http) = identity.http() else { return };
        let Some(text) = self.inner.lock().await.text_channel else {
            return;
        };
        let msg = views::error(
            &identity.icons(),
            "Couldn't play a track",
            &format!(
                "**{}** · {}\n-# {err}",
                ui::fmt::escape_md(&item.track.title),
                ui::fmt::escape_md(&item.track.artist)
            ),
        );
        let _ = ui::send::post(&http, text, msg).await;
    }

    async fn after_change(&self) {
        self.controller_wake.notify_one();
    }

    async fn persist(&self, settings: GuildSettings) {
        if let Ok(identity) = self.identity() {
            if let Err(e) = settings::save_guild(&identity.state.db, &settings).await {
                tracing::warn!(error = %e, "saving guild settings");
            }
        }
    }

    // ---- controller message ----------------------------------------------------------------------

    fn spawn_controller_task(self: &Arc<Self>) {
        let weak = Arc::downgrade(self);
        tokio::spawn(async move {
            loop {
                let Some(player) = weak.upgrade() else { break };
                let cancel = match player.identity() {
                    Ok(i) => i.cancel.clone(),
                    Err(_) => break,
                };
                drop(player);
                let Some(player) = weak.upgrade() else { break };
                tokio::select! {
                    _ = player.controller_wake.notified() => {}
                    _ = cancel.cancelled() => break,
                }
                tokio::time::sleep(CONTROLLER_COALESCE).await;
                player.render_controller().await;
            }
        });
    }

    /// Post or edit the now-playing controller to match the current state.
    async fn render_controller(self: &Arc<Self>) {
        let Ok(identity) = self.identity() else {
            return;
        };
        let Some(http) = identity.http() else { return };
        let (text, controller, repost) = {
            let s = self.inner.lock().await;
            if s.voice_channel.is_none() {
                return;
            }
            (
                s.text_channel,
                s.controller,
                s.settings.announce
                    && s.settings.announce_after > 0
                    && s.messages_since_controller >= s.settings.announce_after,
            )
        };
        let Some(text) = text else { return };
        let snap = self.snapshot().await;
        let reuse = controller.is_some() && !repost;
        let msg = match &snap.current {
            Some(_) => views::now_playing(&snap, reuse),
            None => views::idle(&snap),
        };

        let mut sent: Option<serenity::all::Message> = None;
        match controller {
            Some((ch, id)) if !repost => match ui::send::edit(&http, ch, id, msg.clone()).await {
                Ok(m) => sent = Some(m),
                Err(e) => {
                    tracing::debug!(guild = %self.guild_id, error = %e, "editing controller; re-posting");
                    // Whatever went wrong with the edit, one controller per guild: drop the old
                    // message before a new one goes up.
                    let _ = ui::send::delete(&http, ch, id).await;
                }
            },
            Some((ch, id)) => {
                let _ = ui::send::delete(&http, ch, id).await;
            }
            None => {}
        }
        if sent.is_none() {
            // A failed edit may have been the kept-attachment path against a message that lost
            // it; a fresh post always uploads.
            let msg = match &snap.current {
                Some(_) if reuse => views::now_playing(&snap, false),
                _ => msg,
            };
            match ui::send::post(&http, text, msg).await {
                Ok(posted) => {
                    let mut s = self.inner.lock().await;
                    s.controller = Some((text, posted.id));
                    s.messages_since_controller = 0;
                    s.settings.controller_channel_id = Some(text.to_string());
                    s.settings.controller_message_id = Some(posted.id.to_string());
                    let settings = s.settings.clone();
                    drop(s);
                    self.persist(settings).await;
                    sent = Some(posted);
                }
                Err(e) => {
                    tracing::warn!(guild = %self.guild_id, error = %e, "posting controller");
                }
            }
        }
        if let Some(m) = sent {
            self.remember_cover_attachment(&m).await;
        }
    }

    /// After the controller has been sent, note the id Discord gave the cover upload so the next
    /// edit keeps it rather than uploading it again.
    async fn remember_cover_attachment(&self, sent: &serenity::all::Message) {
        let mut s = self.inner.lock().await;
        if let Some(cover) = s.current.as_mut().and_then(|c| c.cover.as_mut()) {
            if let Some(a) = sent
                .attachments
                .iter()
                .find(|a| a.filename == cover.filename)
            {
                cover.attachment_id = Some(a.id.get());
            }
        }
    }
}

fn effective_volume(s: &PlayerState, facts: &TrackFacts) -> f32 {
    let user = s.volume as f32 / 100.0;
    let rg = if s.normalize {
        source::replaygain_multiplier(facts.gain_db.map(f64::from), facts.peak.map(f64::from))
    } else {
        1.0
    };
    user * rg
}

/// songbird event handler: a track ended (or errored). Carries the epoch so a stale event cannot
/// advance the queue for a track that already replaced it.
struct TrackEnd {
    player: Weak<GuildPlayer>,
    epoch: u64,
}

#[async_trait]
impl EventHandler for TrackEnd {
    async fn act(&self, ctx: &EventContext<'_>) -> Option<Event> {
        let errored = match ctx {
            EventContext::Track(list) => list
                .iter()
                .any(|(state, _)| matches!(state.playing, PlayMode::Errored(_))),
            _ => false,
        };
        if let Some(player) = self.player.upgrade() {
            let epoch = self.epoch;
            tokio::spawn(async move { player.on_track_end(epoch, errored).await });
        }
        // One shot: the handle is gone with the track anyway.
        Some(Event::Cancel)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loop_cycles() {
        assert_eq!(LoopMode::Off.cycle(), LoopMode::Track);
        assert_eq!(LoopMode::Track.cycle(), LoopMode::Queue);
        assert_eq!(LoopMode::Queue.cycle(), LoopMode::Off);
    }

    fn row(id: &str, duration_ms: i64) -> Arc<TrackRow> {
        Arc::new(TrackRow {
            id: id.into(),
            library_id: String::new(),
            content_hash: format!("h{id}"),
            title: format!("Track {id}"),
            artist: "Artist".into(),
            album_artist: None,
            album: None,
            year: None,
            genre: None,
            track_no: None,
            disc_no: None,
            duration_ms,
            acoustid: None,
            recording_mbid: None,
            artist_norm: "artist".into(),
            title_norm: format!("track {id}"),
            album_norm: None,
            codec: "flac".into(),
            sample_rate_hz: 44100,
            bit_depth: 16,
            channels: 2,
            lossless: 1,
            spatial: 0,
            rg_gain_db: None,
            rg_peak: None,
        })
    }

    #[test]
    fn snapshot_eta_and_duration() {
        let item = |id: &str, d: i64| QueueItem {
            track: row(id, d),
            requested_by: UserId::new(1),
            autoplay: false,
        };
        let snap = PlayerSnapshot {
            bot_index: 0,
            bot_name: "Chordia".into(),
            icons: Arc::new(IconSet::default()),
            web_base: None,
            guild_id: GuildId::new(1),
            voice_channel: None,
            voice_channel_name: None,
            current: Some(CurrentSnapshot {
                item: item("c", 100_000),
                facts: TrackFacts::from_row(&row("c", 100_000)),
                position_ms: 40_000,
                paused: false,
                cover: None,
                links: None,
            }),
            queue: vec![item("a", 10_000), item("b", 20_000)],
            history: vec![],
            loop_mode: LoopMode::Off,
            autoplay: false,
            volume: 100,
            normalize: true,
            listeners: 0,
            shuffle: false,
        };
        assert_eq!(snap.queue_duration_ms(), 30_000);
        assert_eq!(snap.eta_ms(0), 60_000);
        assert_eq!(snap.eta_ms(1), 70_000);
    }

    #[test]
    fn effective_volume_applies_replaygain_only_when_normalizing() {
        let mut facts = TrackFacts::from_row(&row("x", 1));
        // −6 dB after the preamp.
        facts.gain_db = Some((-6.0 - source::REPLAYGAIN_PREAMP_DB) as f32);
        let mk = |volume: u8, normalize: bool| PlayerState {
            voice_channel: None,
            text_channel: None,
            queue: VecDeque::new(),
            history: VecDeque::new(),
            current: None,
            loop_mode: LoopMode::Off,
            autoplay: false,
            shuffle: false,
            volume,
            normalize,
            listeners: HashSet::new(),
            idle_since: None,
            alone_since: None,
            controller: None,
            messages_since_controller: 0,
            now_playing_for: Vec::new(),
            now_playing_at: None,
            bitrate_kbps: None,
            epoch: 0,
            next_override: None,
            skip_requested: false,
            stopping: false,
            settings: GuildSettings::defaults("1", "2"),
        };
        assert!((effective_volume(&mk(100, false), &facts) - 1.0).abs() < 1e-6);
        let v = effective_volume(&mk(100, true), &facts);
        assert!((v - 0.501).abs() < 0.01, "{v}");
        let v = effective_volume(&mk(50, true), &facts);
        assert!((v - 0.25).abs() < 0.01, "{v}");
    }
}
