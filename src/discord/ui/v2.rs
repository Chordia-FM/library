//! A typed builder for Discord **Components V2** messages that serializes to the raw JSON Discord
//! expects.
//!
//! serenity 0.12 predates Components V2: it has no `Container`/`Section`/`TextDisplay` builders,
//! and no `IS_COMPONENTS_V2` flag. What it does have is `Http` methods that accept any
//! `impl Serialize` as the body (and it skips unknown top-level component kinds when it parses the
//! messages back), so the bot owns the JSON shape here, in one place. When serenity ships typed V2
//! builders, this module is the only thing that changes.
//!
//! Rules Discord enforces, enforced here by [`Message::validate`] so they fail in a test rather
//! than as a 400 in a voice channel: at most 40 components per message (nested ones count), no
//! `content`/`embeds` alongside the V2 flag, a `TextDisplay` holds at most 4000 characters, a select
//! menu offers at most 25 options, an action row holds at most 5 buttons or exactly 1 select, and a
//! `Thumbnail` is only valid as a `Section` accessory. Attachments are invisible under the V2 flag
//! unless a `Thumbnail`/`MediaGallery` references them as `attachment://<filename>`.

use serde_json::{json, Value};
use serenity::builder::CreateAttachment;

/// `IS_COMPONENTS_V2` — once a message is sent with it, it can never be removed.
pub const FLAG_COMPONENTS_V2: u64 = 1 << 15;
/// `EPHEMERAL` — only the invoking user sees the message.
pub const FLAG_EPHEMERAL: u64 = 1 << 6;

pub const MAX_COMPONENTS: usize = 40;
pub const MAX_TEXT_DISPLAY_CHARS: usize = 4000;
pub const MAX_SELECT_OPTIONS: usize = 25;
pub const MAX_BUTTONS_PER_ROW: usize = 5;

/// An emoji as Discord addresses it: a Unicode glyph, or a custom one by id (for the bot, one of
/// its own application emojis).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Emoji {
    Unicode(String),
    Custom { name: String, id: u64 },
}

impl Emoji {
    /// How the emoji is written inside message text.
    pub fn markup(&self) -> String {
        match self {
            Emoji::Unicode(s) => s.clone(),
            Emoji::Custom { name, id } => format!("<:{name}:{id}>"),
        }
    }

    /// The `emoji` object on a button or select option.
    pub fn to_json(&self) -> Value {
        match self {
            Emoji::Unicode(s) => json!({ "name": s }),
            Emoji::Custom { name, id } => json!({ "id": id.to_string(), "name": name }),
        }
    }
}

impl From<&str> for Emoji {
    fn from(s: &str) -> Self {
        Emoji::Unicode(s.to_string())
    }
}

impl From<String> for Emoji {
    fn from(s: String) -> Self {
        Emoji::Unicode(s)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ButtonStyle {
    Primary = 1,
    Secondary = 2,
    Success = 3,
    Danger = 4,
    Link = 5,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Button {
    pub style: ButtonStyle,
    pub label: Option<String>,
    /// A Unicode glyph, or one of the bot's own application emojis (never a guild's emoji: the
    /// bot serves many guilds and those do not travel).
    pub emoji: Option<Emoji>,
    pub custom_id: Option<String>,
    pub url: Option<String>,
    pub disabled: bool,
}

impl Button {
    pub fn new(style: ButtonStyle, custom_id: impl Into<String>) -> Self {
        Self {
            style,
            label: None,
            emoji: None,
            custom_id: Some(custom_id.into()),
            url: None,
            disabled: false,
        }
    }

    pub fn link(url: impl Into<String>, label: impl Into<String>) -> Self {
        Self {
            style: ButtonStyle::Link,
            label: Some(label.into()),
            emoji: None,
            custom_id: None,
            url: Some(url.into()),
            disabled: false,
        }
    }

    pub fn label(mut self, label: impl Into<String>) -> Self {
        self.label = Some(label.into());
        self
    }

    pub fn emoji(mut self, emoji: impl Into<Emoji>) -> Self {
        self.emoji = Some(emoji.into());
        self
    }

    pub fn disabled(mut self, disabled: bool) -> Self {
        self.disabled = disabled;
        self
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SelectOption {
    pub label: String,
    pub value: String,
    pub description: Option<String>,
    pub emoji: Option<Emoji>,
    pub default: bool,
}

impl SelectOption {
    pub fn new(label: impl Into<String>, value: impl Into<String>) -> Self {
        Self {
            label: clip(label.into(), 100),
            value: value.into(),
            description: None,
            emoji: None,
            default: false,
        }
    }

    pub fn description(mut self, d: impl Into<String>) -> Self {
        self.description = Some(clip(d.into(), 100));
        self
    }

    pub fn emoji(mut self, e: impl Into<Emoji>) -> Self {
        self.emoji = Some(e.into());
        self
    }

    pub fn default(mut self, d: bool) -> Self {
        self.default = d;
        self
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Spacing {
    Small = 1,
    Large = 2,
}

/// An image reference: `attachment://<filename>` for a file uploaded with the message, or an
/// `https://` URL Discord fetches itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Media {
    pub url: String,
}

impl Media {
    pub fn attachment(filename: &str) -> Self {
        Self {
            url: format!("attachment://{filename}"),
        }
    }

    pub fn url(url: impl Into<String>) -> Self {
        Self { url: url.into() }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Component {
    ActionRow(Vec<Component>),
    Button(Button),
    StringSelect {
        custom_id: String,
        placeholder: Option<String>,
        options: Vec<SelectOption>,
        min: u8,
        max: u8,
        disabled: bool,
    },
    RoleSelect {
        custom_id: String,
        placeholder: Option<String>,
        default_roles: Vec<u64>,
        /// How many may be picked at once (Discord allows up to 25).
        max_values: u8,
    },
    TextDisplay(String),
    Section {
        components: Vec<Component>,
        accessory: Box<Component>,
    },
    Thumbnail {
        media: Media,
        description: Option<String>,
    },
    MediaGallery(Vec<Media>),
    Separator {
        divider: bool,
        spacing: Spacing,
    },
    Container {
        accent: Option<u32>,
        components: Vec<Component>,
    },
}

// Short constructors so views read like the layout they produce.

pub fn text(s: impl Into<String>) -> Component {
    Component::TextDisplay(s.into())
}

pub fn row(components: Vec<Component>) -> Component {
    Component::ActionRow(components)
}

pub fn button(b: Button) -> Component {
    Component::Button(b)
}

pub fn separator(divider: bool, spacing: Spacing) -> Component {
    Component::Separator { divider, spacing }
}

pub fn section(components: Vec<Component>, accessory: Component) -> Component {
    Component::Section {
        components,
        accessory: Box::new(accessory),
    }
}

pub fn thumbnail(media: Media, description: Option<String>) -> Component {
    Component::Thumbnail { media, description }
}

pub fn gallery(items: Vec<Media>) -> Component {
    Component::MediaGallery(items)
}

pub fn container(accent: u32, components: Vec<Component>) -> Component {
    Component::Container {
        accent: Some(accent),
        components,
    }
}

impl Component {
    pub fn to_json(&self) -> Value {
        match self {
            Component::ActionRow(items) => json!({
                "type": 1,
                "components": items.iter().map(Component::to_json).collect::<Vec<_>>(),
            }),
            Component::Button(b) => {
                let mut v = json!({ "type": 2, "style": b.style as u8, "disabled": b.disabled });
                if let Some(l) = &b.label {
                    v["label"] = json!(clip(l.clone(), 80));
                }
                if let Some(e) = &b.emoji {
                    v["emoji"] = e.to_json();
                }
                if let Some(id) = &b.custom_id {
                    v["custom_id"] = json!(id);
                }
                if let Some(u) = &b.url {
                    v["url"] = json!(u);
                }
                v
            }
            Component::StringSelect {
                custom_id,
                placeholder,
                options,
                min,
                max,
                disabled,
            } => {
                let mut v = json!({
                    "type": 3,
                    "custom_id": custom_id,
                    "min_values": min,
                    "max_values": max,
                    "disabled": disabled,
                    "options": options.iter().map(|o| {
                        let mut ov = json!({ "label": o.label, "value": o.value, "default": o.default });
                        if let Some(d) = &o.description {
                            ov["description"] = json!(d);
                        }
                        if let Some(e) = &o.emoji {
                            ov["emoji"] = e.to_json();
                        }
                        ov
                    }).collect::<Vec<_>>(),
                });
                if let Some(p) = placeholder {
                    v["placeholder"] = json!(clip(p.clone(), 150));
                }
                v
            }
            Component::RoleSelect {
                custom_id,
                placeholder,
                default_roles,
                max_values,
            } => {
                let mut v = json!({
                    "type": 6,
                    "custom_id": custom_id,
                    "min_values": 0,
                    "max_values": (*max_values).clamp(1, 25),
                });
                if let Some(p) = placeholder {
                    v["placeholder"] = json!(clip(p.clone(), 150));
                }
                if !default_roles.is_empty() {
                    let defaults: Vec<Value> = default_roles
                        .iter()
                        .take(25)
                        .map(|r| json!({ "id": r.to_string(), "type": "role" }))
                        .collect();
                    v["default_values"] = Value::Array(defaults);
                }
                v
            }
            Component::TextDisplay(s) => json!({ "type": 10, "content": s }),
            Component::Section {
                components,
                accessory,
            } => json!({
                "type": 9,
                "components": components.iter().map(Component::to_json).collect::<Vec<_>>(),
                "accessory": accessory.to_json(),
            }),
            Component::Thumbnail { media, description } => {
                let mut v = json!({ "type": 11, "media": { "url": media.url } });
                if let Some(d) = description {
                    v["description"] = json!(clip(d.clone(), 1024));
                }
                v
            }
            Component::MediaGallery(items) => json!({
                "type": 12,
                "items": items.iter().map(|m| json!({ "media": { "url": m.url } })).collect::<Vec<_>>(),
            }),
            Component::Separator { divider, spacing } => json!({
                "type": 14,
                "divider": divider,
                "spacing": *spacing as u8,
            }),
            Component::Container { accent, components } => {
                let mut v = json!({
                    "type": 17,
                    "components": components.iter().map(Component::to_json).collect::<Vec<_>>(),
                });
                if let Some(a) = accent {
                    v["accent_color"] = json!(a);
                }
                v
            }
        }
    }

    /// This component plus everything nested in it — Discord's 40 counts them all.
    pub fn count(&self) -> usize {
        1 + match self {
            Component::ActionRow(items)
            | Component::Container {
                components: items, ..
            } => items.iter().map(Component::count).sum(),
            Component::Section {
                components,
                accessory,
            } => components.iter().map(Component::count).sum::<usize>() + accessory.count(),
            _ => 0,
        }
    }

    fn validate(&self, top_level: bool, errors: &mut Vec<String>) {
        match self {
            Component::ActionRow(items) => {
                let buttons = items
                    .iter()
                    .filter(|c| matches!(c, Component::Button(_)))
                    .count();
                let selects = items.len() - buttons;
                if buttons > MAX_BUTTONS_PER_ROW {
                    errors.push(format!("action row has {buttons} buttons (max 5)"));
                }
                if selects > 1 || (selects == 1 && buttons > 0) {
                    errors.push("action row mixes a select with other components".into());
                }
                for c in items {
                    match c {
                        Component::Button(_)
                        | Component::StringSelect { .. }
                        | Component::RoleSelect { .. } => c.validate(false, errors),
                        other => {
                            errors.push(format!("{} is not allowed in an action row", other.name()))
                        }
                    }
                }
            }
            Component::Button(b) => {
                if b.label.is_none() && b.emoji.is_none() {
                    errors.push("button has neither label nor emoji".into());
                }
                if b.style == ButtonStyle::Link {
                    if b.url.is_none() || b.custom_id.is_some() {
                        errors.push("link button needs a url and no custom_id".into());
                    }
                } else if b.custom_id.is_none() {
                    errors.push("button has no custom_id".into());
                }
                if b.custom_id.as_ref().is_some_and(|id| id.len() > 100) {
                    errors.push("custom_id longer than 100".into());
                }
            }
            Component::StringSelect { options, .. } => {
                if options.is_empty() {
                    errors.push("select has no options".into());
                }
                if options.len() > MAX_SELECT_OPTIONS {
                    errors.push(format!("select has {} options (max 25)", options.len()));
                }
            }
            Component::RoleSelect { .. } => {}
            Component::TextDisplay(s) => {
                if s.chars().count() > MAX_TEXT_DISPLAY_CHARS {
                    errors.push("text display over 4000 characters".into());
                }
                if s.is_empty() {
                    errors.push("empty text display".into());
                }
            }
            Component::Section {
                components,
                accessory,
            } => {
                if components.is_empty() || components.len() > 3 {
                    errors.push("section needs 1–3 components".into());
                }
                for c in components {
                    if !matches!(c, Component::TextDisplay(_)) {
                        errors.push("section may only hold text displays".into());
                    }
                    c.validate(false, errors);
                }
                match accessory.as_ref() {
                    Component::Thumbnail { .. } | Component::Button(_) => {}
                    other => errors.push(format!("{} cannot be a section accessory", other.name())),
                }
                accessory.validate(false, errors);
            }
            Component::Thumbnail { .. } => {
                if top_level {
                    errors.push("thumbnail only lives in a section accessory".into());
                }
            }
            Component::MediaGallery(items) => {
                if items.is_empty() || items.len() > 10 {
                    errors.push("media gallery needs 1–10 items".into());
                }
            }
            Component::Separator { .. } => {}
            Component::Container { components, .. } => {
                if components.is_empty() {
                    errors.push("empty container".into());
                }
                for c in components {
                    if matches!(c, Component::Container { .. }) {
                        errors.push("container nested in a container".into());
                    }
                    c.validate(true, errors);
                }
            }
        }
    }

    fn name(&self) -> &'static str {
        match self {
            Component::ActionRow(_) => "action row",
            Component::Button(_) => "button",
            Component::StringSelect { .. } => "string select",
            Component::RoleSelect { .. } => "role select",
            Component::TextDisplay(_) => "text display",
            Component::Section { .. } => "section",
            Component::Thumbnail { .. } => "thumbnail",
            Component::MediaGallery(_) => "media gallery",
            Component::Separator { .. } => "separator",
            Component::Container { .. } => "container",
        }
    }
}

#[derive(Debug, thiserror::Error)]
#[error("invalid components v2 message: {0}")]
pub struct UiError(pub String);

/// A complete V2 message: components, the files they reference, and delivery flags.
#[derive(Debug, Default, Clone)]
pub struct Message {
    pub components: Vec<Component>,
    /// Files uploaded with this message. Their index is their attachment id in the payload, which
    /// is how `attachment://<filename>` resolves.
    pub attachments: Vec<CreateAttachment>,
    /// Attachment ids already on the message that an edit should keep. Discord drops any existing
    /// attachment an edit omits.
    pub keep_attachments: Vec<u64>,
    pub ephemeral: bool,
}

impl Message {
    pub fn new(components: Vec<Component>) -> Self {
        Self {
            components,
            ..Default::default()
        }
    }

    pub fn ephemeral(mut self) -> Self {
        self.ephemeral = true;
        self
    }

    pub fn attach(mut self, filename: &str, bytes: Vec<u8>) -> Self {
        self.attachments
            .push(CreateAttachment::bytes(bytes, filename.to_string()));
        self
    }

    pub fn keep_attachment(mut self, id: u64) -> Self {
        self.keep_attachments.push(id);
        self
    }

    pub fn component_count(&self) -> usize {
        self.components.iter().map(Component::count).sum()
    }

    pub fn validate(&self) -> Result<(), UiError> {
        let mut errors = Vec::new();
        if self.components.is_empty() {
            errors.push("message has no components".into());
        }
        let n = self.component_count();
        if n > MAX_COMPONENTS {
            errors.push(format!("{n} components (max 40)"));
        }
        for c in &self.components {
            c.validate(true, &mut errors);
        }
        for a in &self.attachments {
            let referenced = self.references_attachment(&a.filename);
            if !referenced {
                errors.push(format!(
                    "attachment {} is not referenced by any component",
                    a.filename
                ));
            }
        }
        if errors.is_empty() {
            Ok(())
        } else {
            Err(UiError(errors.join("; ")))
        }
    }

    fn references_attachment(&self, filename: &str) -> bool {
        let needle = format!("attachment://{filename}");
        fn walk(c: &Component, needle: &str) -> bool {
            match c {
                Component::Thumbnail { media, .. } => media.url == needle,
                Component::MediaGallery(items) => items.iter().any(|m| m.url == needle),
                Component::ActionRow(items)
                | Component::Container {
                    components: items, ..
                } => items.iter().any(|c| walk(c, needle)),
                Component::Section {
                    components,
                    accessory,
                } => components.iter().any(|c| walk(c, needle)) || walk(accessory, needle),
                _ => false,
            }
        }
        self.components.iter().any(|c| walk(c, &needle))
    }

    /// The JSON body for a create/edit request. Mentions are never parsed: a track title is user
    /// data, and `@everyone` in an ID3 tag must stay text.
    pub fn body(&self) -> Value {
        let mut flags = FLAG_COMPONENTS_V2;
        if self.ephemeral {
            flags |= FLAG_EPHEMERAL;
        }
        let mut attachments: Vec<Value> = self
            .keep_attachments
            .iter()
            .map(|id| json!({ "id": id.to_string() }))
            .collect();
        attachments.extend(
            self.attachments
                .iter()
                .enumerate()
                .map(|(i, a)| json!({ "id": i, "filename": a.filename })),
        );
        json!({
            "flags": flags,
            "components": self.components.iter().map(Component::to_json).collect::<Vec<_>>(),
            "allowed_mentions": { "parse": [] },
            "attachments": attachments,
        })
    }

    /// The files to upload, leaving the message reusable for a later edit.
    pub fn take_attachments(&mut self) -> Vec<CreateAttachment> {
        std::mem::take(&mut self.attachments)
    }
}

/// Truncate to `max` characters with an ellipsis, on a character boundary.
pub fn clip(s: String, max: usize) -> String {
    if s.chars().count() <= max {
        return s;
    }
    let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Message {
        Message::new(vec![container(
            0xff00aa,
            vec![
                text("### ▶ Now playing"),
                separator(true, Spacing::Small),
                section(
                    vec![text("**Title**\nArtist")],
                    thumbnail(Media::attachment("cover.png"), None),
                ),
                gallery(vec![Media::attachment("card.png")]),
                row(vec![
                    button(Button::new(ButtonStyle::Primary, "cd:1:0:1:pl").emoji("⏯")),
                    button(Button::new(ButtonStyle::Secondary, "cd:1:0:1:sk").label("Skip")),
                ]),
            ],
        )])
        .attach("cover.png", vec![1, 2, 3])
        .attach("card.png", vec![4])
    }

    #[test]
    fn body_has_v2_flag_no_content_and_sequential_attachment_ids() {
        let m = sample();
        m.validate().unwrap();
        let b = m.body();
        assert_eq!(b["flags"], json!(FLAG_COMPONENTS_V2));
        assert!(b.get("content").is_none());
        assert!(b.get("embeds").is_none());
        assert_eq!(b["allowed_mentions"]["parse"], json!([]));
        assert_eq!(b["attachments"][0]["id"], json!(0));
        assert_eq!(b["attachments"][0]["filename"], json!("cover.png"));
        assert_eq!(b["attachments"][1]["id"], json!(1));
        assert_eq!(b["components"][0]["type"], json!(17));
        assert_eq!(b["components"][0]["accent_color"], json!(0xff00aa));
        assert_eq!(b["components"][0]["components"][0]["type"], json!(10));
        assert_eq!(b["components"][0]["components"][2]["type"], json!(9));
        assert_eq!(
            b["components"][0]["components"][2]["accessory"]["type"],
            json!(11)
        );
        assert_eq!(b["components"][0]["components"][3]["type"], json!(12));
        assert_eq!(b["components"][0]["components"][4]["type"], json!(1));
        assert_eq!(
            b["components"][0]["components"][4]["components"][0]["emoji"]["name"],
            json!("⏯")
        );
    }

    #[test]
    fn ephemeral_adds_flag_and_kept_attachments_are_strings() {
        let m = Message::new(vec![text("hi")])
            .ephemeral()
            .keep_attachment(123);
        let b = m.body();
        assert_eq!(b["flags"], json!(FLAG_COMPONENTS_V2 | FLAG_EPHEMERAL));
        assert_eq!(b["attachments"][0]["id"], json!("123"));
    }

    #[test]
    fn counts_nested_components() {
        assert_eq!(sample().component_count(), 10);
    }

    #[test]
    fn rejects_too_many_components() {
        let many: Vec<Component> = (0..41).map(|i| text(format!("{i}"))).collect();
        let m = Message::new(many);
        assert!(m.validate().unwrap_err().0.contains("max 40"));
    }

    #[test]
    fn rejects_long_text_and_many_options_and_bad_rows() {
        let long = "x".repeat(4001);
        assert!(Message::new(vec![text(long)]).validate().is_err());

        let options: Vec<SelectOption> = (0..26)
            .map(|i| SelectOption::new(format!("o{i}"), format!("v{i}")))
            .collect();
        let m = Message::new(vec![row(vec![Component::StringSelect {
            custom_id: "x".into(),
            placeholder: None,
            options,
            min: 1,
            max: 1,
            disabled: false,
        }])]);
        assert!(m.validate().unwrap_err().0.contains("max 25"));

        let six: Vec<Component> = (0..6)
            .map(|i| button(Button::new(ButtonStyle::Secondary, format!("b{i}")).label("b")))
            .collect();
        assert!(Message::new(vec![row(six)]).validate().is_err());

        let m = Message::new(vec![thumbnail(Media::attachment("a.png"), None)]);
        assert!(m.validate().is_err());
    }

    #[test]
    fn rejects_unreferenced_attachment() {
        let m = Message::new(vec![text("hi")]).attach("x.png", vec![0]);
        assert!(m.validate().unwrap_err().0.contains("not referenced"));
    }

    #[test]
    fn clip_is_char_safe() {
        assert_eq!(clip("héllo".into(), 10), "héllo");
        assert_eq!(clip("héllo wörld".into(), 6), "héllo…");
    }
}
