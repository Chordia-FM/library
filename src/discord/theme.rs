//! The bot's look on Discord as one persistent job: the icon set and the avatar in one colour.
//!
//! Settings hold what the owner wants (`emoji_hex`, `avatar_managed`, a custom avatar file) and
//! what Discord currently has (`emoji_hex_applied`, `avatar_hex_applied`,
//! `avatar_custom_applied`). [`tick`] closes the gap when there is one. It runs on connect, on
//! every ticker beat, and right after a dashboard change, so a change is usually applied within
//! seconds and never lost if it cannot be.
//!
//! Discord rate-limits both halves (a few avatar changes an hour; an emoji bucket that a full
//! recolour, 26 deletes and 26 uploads, can exhaust when repeated). A 429 is not an error here: the
//! retry time it names is stored as `theme_retry_at` with a margin, a warning is stored beside it
//! for the dashboard to show, the controls stay disabled until then, and the next tick past that
//! time applies whatever is still pending. Repeated limits widen the margin.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use serde::Serialize;

use crate::discord::avatar;
use crate::discord::emoji::{self, IconSet, DEFAULT_HEX};
use crate::discord::identity::Identity;
use crate::discord::rest::{Rest, RestError};
use crate::discord::settings::{now_ms, BotSettings};

/// What the dashboard shows and disables on.
#[derive(Debug, Clone, Serialize)]
pub struct ThemeStatus {
    /// The colour in effect (or wanted).
    pub hex: String,
    pub emoji_applied: bool,
    pub avatar_managed: bool,
    pub avatar_applied: bool,
    /// Something is still waiting to reach Discord.
    pub pending: bool,
    /// Epoch millis after which the next attempt may run; controls stay locked until then.
    pub retry_at: Option<i64>,
    pub warning: Option<String>,
}

pub fn status(s: &BotSettings) -> ThemeStatus {
    let hex = desired_hex(s);
    let emoji_applied = s.emoji_hex_applied.as_deref() == Some(hex.as_str());
    let avatar_applied = if s.avatar_managed {
        s.avatar_hex_applied.as_deref() == Some(hex.as_str())
    } else {
        s.avatar_custom_path.is_none() || s.avatar_custom_applied
    };
    ThemeStatus {
        pending: !(emoji_applied && avatar_applied),
        hex,
        emoji_applied,
        avatar_managed: s.avatar_managed,
        avatar_applied,
        retry_at: s.theme_retry_at.filter(|t| *t > now_ms()),
        warning: s.theme_warning.clone(),
    }
}

pub fn desired_hex(s: &BotSettings) -> String {
    s.emoji_hex
        .as_deref()
        .and_then(emoji::normalize_hex)
        .unwrap_or_else(|| DEFAULT_HEX.to_string())
}

/// Where an owner-uploaded avatar is kept until (and after) it reaches Discord.
pub fn custom_avatar_path(identity: &Identity, ext: &str) -> PathBuf {
    identity.state.config.data_dir.join("discord").join(format!(
        "{}-avatar.{ext}",
        identity.app_id_sync().unwrap_or(0)
    ))
}

/// Bring Discord in line with the settings. Safe to call often: it does nothing when nothing is
/// pending (beyond one emoji listing on the first call after connect, to learn the ids), and
/// nothing at all while a rate limit is in force.
pub async fn tick(identity: &Arc<Identity>) {
    let Ok(_guard) = identity.theme_lock.try_lock() else {
        return;
    };
    if identity.http().is_none() {
        return;
    }
    let mut s = identity.settings();
    if s.theme_retry_at.is_some_and(|t| now_ms() < t) {
        return;
    }
    let hex = desired_hex(&s);
    let rest = Rest::new(identity.state.http.clone(), identity.token.clone());

    // Icons: list on every connect (cheap), rebuild only when the colour moved or something is
    // missing.
    if identity.icons().is_empty() || s.emoji_hex_applied.as_deref() != Some(hex.as_str()) {
        let replace = s.emoji_hex_applied.as_deref() != Some(hex.as_str());
        match emoji::provision(&rest, &hex, replace).await {
            Ok(set) => {
                tracing::info!(bot = identity.index, icons = set.len(), colour = %hex, replaced = replace, "application emojis ready");
                identity.set_icons(Arc::new(set));
                s.emoji_hex_applied = Some(hex.clone());
                s.theme_warning = None;
                s.theme_retry_at = None;
                s.theme_backoff = 0;
            }
            Err(e) => {
                identity.set_icons(Arc::new(IconSet::default().with_accent(&hex)));
                note_failure(identity.index, &mut s, "icon set", e);
                persist(identity, s).await;
                return;
            }
        }
    }

    // Avatar: the mark in the colour, or the owner's file once.
    let avatar_job: Option<(String, Vec<u8>, &'static str)> = if s.avatar_managed {
        if s.avatar_hex_applied.as_deref() != Some(hex.as_str()) {
            match avatar::render_png(&hex) {
                Ok(png) => Some(("image/png".to_string(), png, "mark")),
                Err(e) => {
                    tracing::warn!(error = %e, "rendering the avatar mark");
                    None
                }
            }
        } else {
            None
        }
    } else if let (Some(path), false) = (&s.avatar_custom_path, s.avatar_custom_applied) {
        match tokio::fs::read(path).await {
            Ok(bytes) => {
                let mime = match PathBuf::from(path)
                    .extension()
                    .and_then(|e| e.to_str())
                    .unwrap_or("png")
                {
                    "jpg" | "jpeg" => "image/jpeg",
                    "gif" => "image/gif",
                    "webp" => "image/webp",
                    _ => "image/png",
                };
                Some((mime.to_string(), bytes, "custom"))
            }
            Err(e) => {
                tracing::warn!(error = %e, path = %path, "reading the custom avatar");
                s.avatar_custom_path = None;
                None
            }
        }
    } else {
        None
    };
    if let Some((mime, bytes, kind)) = avatar_job {
        match avatar::upload(&rest, &mime, &bytes).await {
            Ok(()) => {
                tracing::info!(bot = identity.index, kind, "avatar updated");
                if kind == "mark" {
                    s.avatar_hex_applied = Some(hex.clone());
                } else {
                    s.avatar_custom_applied = true;
                }
                s.theme_warning = None;
                s.theme_retry_at = None;
                s.theme_backoff = 0;
            }
            Err(e) => note_failure(identity.index, &mut s, "avatar", e),
        }
    }
    persist(identity, s).await;
}

/// Turn a failure into state the dashboard can show and the ticker can act on.
fn note_failure(bot: u8, s: &mut BotSettings, what: &str, e: RestError) {
    match e {
        RestError::RateLimited {
            retry_after,
            global,
        } => {
            // Widen the margin each time a limit hits back to back; Discord's number is a floor,
            // not a promise.
            s.theme_backoff = (s.theme_backoff + 1).min(6);
            let margin = Duration::from_secs(30 * (1u64 << (s.theme_backoff - 1)));
            let wait = retry_after + margin;
            let at = now_ms() + wait.as_millis() as i64;
            s.theme_retry_at = Some(at);
            s.theme_warning = Some(format!(
                "Discord rate-limited the {what} change{}; it is queued and will be applied \
                 automatically in about {}. Changes are locked until then.",
                if global { " (globally)" } else { "" },
                human(wait)
            ));
            tracing::warn!(
                bot,
                what,
                wait_secs = wait.as_secs(),
                "theme change rate-limited; queued"
            );
        }
        other => {
            s.theme_warning = Some(format!("Discord refused the {what} change: {other}"));
            // Not a limit: try again on the next tick after a short pause rather than hammering.
            s.theme_retry_at = Some(now_ms() + 5 * 60 * 1000);
            tracing::warn!(bot, what, error = %other, "theme change failed");
        }
    }
}

fn human(d: Duration) -> String {
    let s = d.as_secs();
    if s >= 3600 {
        format!("{} h {} min", s / 3600, (s % 3600) / 60)
    } else if s >= 60 {
        format!("{} min", s.div_ceil(60))
    } else {
        format!("{s} s")
    }
}

async fn persist(identity: &Identity, s: BotSettings) {
    identity.set_settings(s);
    identity.save_settings().await;
}

/// The owner picked a colour. Stores it, then applies at once unless a limit is in force.
pub async fn request_hex(identity: &Arc<Identity>, hex: &str) -> ThemeStatus {
    let mut s = identity.settings();
    s.emoji_hex = Some(hex.to_string());
    persist(identity, s).await;
    tick(identity).await;
    status(&identity.settings())
}

/// The owner uploaded their own avatar: keep it, stop managing the mark, apply when allowed.
pub async fn request_custom_avatar(
    identity: &Arc<Identity>,
    ext: &str,
    bytes: &[u8],
) -> anyhow::Result<ThemeStatus> {
    let path = custom_avatar_path(identity, ext);
    if let Some(dir) = path.parent() {
        tokio::fs::create_dir_all(dir).await?;
    }
    tokio::fs::write(&path, bytes).await?;
    let mut s = identity.settings();
    s.avatar_managed = false;
    s.avatar_custom_path = Some(path.to_string_lossy().into_owned());
    s.avatar_custom_applied = false;
    persist(identity, s).await;
    tick(identity).await;
    Ok(status(&identity.settings()))
}

/// Back to the managed mark (re-applied at the next opportunity).
pub async fn request_managed_avatar(identity: &Arc<Identity>) -> ThemeStatus {
    let mut s = identity.settings();
    s.avatar_managed = true;
    s.avatar_hex_applied = None;
    persist(identity, s).await;
    tick(identity).await;
    status(&identity.settings())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_reflects_the_gaps() {
        let mut s = BotSettings::defaults("1");
        let st = status(&s);
        assert_eq!(st.hex, DEFAULT_HEX);
        assert!(st.pending && !st.emoji_applied && !st.avatar_applied);

        s.emoji_hex_applied = Some(DEFAULT_HEX.into());
        s.avatar_hex_applied = Some(DEFAULT_HEX.into());
        assert!(!status(&s).pending);

        s.emoji_hex = Some("#e67451".into());
        let st = status(&s);
        assert_eq!(st.hex, "#e67451");
        assert!(st.pending);

        // A custom avatar that reached Discord is "applied" regardless of colour.
        s.avatar_managed = false;
        s.avatar_custom_path = Some("x.png".into());
        s.avatar_custom_applied = true;
        assert!(status(&s).avatar_applied);
        s.avatar_custom_applied = false;
        assert!(!status(&s).avatar_applied);
    }

    #[test]
    fn rate_limits_lock_and_widen() {
        let mut s = BotSettings::defaults("1");
        note_failure(
            0,
            &mut s,
            "icon set",
            RestError::RateLimited {
                retry_after: Duration::from_secs(600),
                global: false,
            },
        );
        let first = s.theme_retry_at.unwrap();
        assert!(first > now_ms() + 600_000);
        assert!(s.theme_warning.as_deref().unwrap().contains("queued"));
        assert_eq!(s.theme_backoff, 1);
        note_failure(
            0,
            &mut s,
            "avatar",
            RestError::RateLimited {
                retry_after: Duration::from_secs(600),
                global: true,
            },
        );
        assert_eq!(s.theme_backoff, 2);
        assert!(s.theme_retry_at.unwrap() >= first);
        assert!(s.theme_warning.as_deref().unwrap().contains("globally"));
    }

    #[test]
    fn durations_read_naturally() {
        assert_eq!(human(Duration::from_secs(45)), "45 s");
        assert_eq!(human(Duration::from_secs(61)), "2 min");
        assert_eq!(human(Duration::from_secs(3700)), "1 h 1 min");
    }
}
