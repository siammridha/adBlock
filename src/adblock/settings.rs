//! Which rules Adblock is allowed to act on, and where that choice is kept.
//!
//! One master switch: with it off, Adblock matches nothing and every request
//! passes, which is what the dashboard's "Ad blocking" toggle turns. Then three
//! switches for what a block decision may carry: serving a `$redirect`
//! stand-in body in place of a blocked resource, reporting the `$removeparam`
//! cleaned URL, and adding the `$csp` directives to a page. Three more for what
//! Adblock puts into a page it rewrites: cosmetic CSS, uBO scriptlets, and the
//! live-DOM runtime. Then the picture blur, whose switches follow HaramBlur's
//! one for one: its own switch, a switch each for blurring men and blurring
//! women, a switch each for looking at images and at videos, a switch for
//! covering only the people rather than the whole picture, a switch for draining
//! the colour out along with the blur, a switch for blurring media on load until
//! a verdict arrives, a switch each for lifting the blur while the pointer is
//! over an image and over a video, how hard to blur and how sure the detector
//! has to be.
//!
//! Then the ones that are ours rather than HaramBlur's: what size a picture is
//! shrunk to before it is looked at — with a switch to not shrink it at all — a
//! size below which a picture is not worth looking at, with its own switch,
//! whether to mark up what was checked, and which of the runtime's detectors to
//! load.
//!
//! All of them belong to Adblock rather than the caller. The caller hands over a
//! request or a response and takes back what Adblock made of it; it never asks
//! for a rule to be applied, so it has nothing to switch off.
//!
//! Persisted to Adblock's own settings file. Nothing else writes that file, so
//! saving rewrites it whole.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU16, Ordering};

use serde::{Deserialize, Serialize};

/// Which detector the blur runtime loads. A closed list, because each name is a
/// worker the runtime already carries — a name that is not one of these is not a
/// model that could be fetched, it is a typo.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum BlurModel {
    /// The Human library on its own: faces, with man or woman and an age.
    Human,
    /// Human finds the faces; a FairFace model reads man or woman off each one.
    HumanFairface,
    /// The same, with rizvandwiki's model in place of FairFace. Same size and
    /// same shape; they disagree about roughly one face in fifteen.
    HumanRizvandwiki,
    /// YOLOS finds whole people; a PETA model reads man or woman off each one.
    /// The only one that answers without needing to see a face.
    PeoplePeta,
}

impl BlurModel {
    const ALL: [Self; 4] =
        [Self::Human, Self::HumanFairface, Self::HumanRizvandwiki, Self::PeoplePeta];

    /// The name the runtime matches on, and the name stored in the settings file.
    pub fn id(self) -> &'static str {
        match self {
            Self::Human => "human",
            Self::HumanFairface => "human-fairface",
            Self::HumanRizvandwiki => "human-rizvandwiki",
            Self::PeoplePeta => "people-peta",
        }
    }

    fn from_id(id: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|m| m.id() == id)
    }

    fn names() -> String {
        Self::ALL.map(Self::id).join(", ")
    }
}

/// A settings update. Callers hand over raw bytes and Adblock decides what is
/// valid; only present, well-typed flags are picked up.
#[derive(Debug, Default, Clone, Copy, Deserialize, Serialize)]
pub struct DecisionOverrides {
    pub enabled: Option<bool>,
    pub redirect: Option<bool>,
    pub removeparam: Option<bool>,
    pub csp: Option<bool>,
    pub cosmetic: Option<bool>,
    pub scriptlets: Option<bool>,
    pub runtime: Option<bool>,
    pub blur: Option<bool>,
    pub blur_men: Option<bool>,
    pub blur_women: Option<bool>,
    pub blur_images: Option<bool>,
    pub blur_videos: Option<bool>,
    pub blur_regions: Option<bool>,
    pub blur_gray: Option<bool>,
    pub blur_on_load: Option<bool>,
    pub blur_hover_images: Option<bool>,
    pub blur_hover_videos: Option<bool>,
    pub blur_marks: Option<bool>,
    pub blur_resize: Option<bool>,
    pub blur_skip_small: Option<bool>,
    /// Blur radius in CSS pixels, 10–50 — HaramBlur's own range.
    pub blur_amount: Option<u8>,
    /// How eager the detector is, 10–100 — HaramBlur's 0.1–1 as a percentage.
    /// Higher blurs on weaker evidence.
    pub blur_strictness: Option<u8>,
    /// Longest side an image is shrunk to before the detector sees it, 32–4096.
    pub blur_img_size: Option<u16>,
    /// The same for a video frame, 32–4096.
    pub blur_video_size: Option<u16>,
    /// A picture with a side under this many pixels is never looked at, 1–4096.
    pub blur_min_size: Option<u16>,
    /// Which detector to load.
    pub blur_model: Option<BlurModel>,
}

impl DecisionOverrides {
    pub fn parse(body: &[u8]) -> std::result::Result<Self, String> {
        let v: serde_json::Value = serde_json::from_slice(body).map_err(|e| e.to_string())?;
        if !v.is_object() {
            return Err("expected a JSON object".into());
        }
        let flag = |key: &str| -> Option<bool> { v.get(key).and_then(serde_json::Value::as_bool) };
        // A number that is present has to be usable: a caller sending 500 for a
        // blur radius has made a mistake, and silently clamping it would hide
        // that. Absent is fine and leaves the setting alone.
        let number = |key: &str, lo: u64, hi: u64| -> std::result::Result<Option<u64>, String> {
            match v.get(key) {
                None | Some(serde_json::Value::Null) => Ok(None),
                Some(n) => match n.as_u64() {
                    Some(n) if (lo..=hi).contains(&n) => Ok(Some(n)),
                    _ => Err(format!("{key} must be a whole number from {lo} to {hi}")),
                },
            }
        };
        Ok(Self {
            enabled: flag("enabled"),
            redirect: flag("redirect"),
            removeparam: flag("removeparam"),
            csp: flag("csp"),
            cosmetic: flag("cosmetic"),
            scriptlets: flag("scriptlets"),
            runtime: flag("runtime"),
            blur: flag("blur"),
            blur_men: flag("blur_men"),
            blur_women: flag("blur_women"),
            blur_images: flag("blur_images"),
            blur_videos: flag("blur_videos"),
            blur_regions: flag("blur_regions"),
            blur_gray: flag("blur_gray"),
            blur_on_load: flag("blur_on_load"),
            blur_hover_images: flag("blur_hover_images"),
            blur_hover_videos: flag("blur_hover_videos"),
            blur_marks: flag("blur_marks"),
            blur_resize: flag("blur_resize"),
            blur_skip_small: flag("blur_skip_small"),
            blur_amount: number("blur_amount", 10, 50)?.map(|n| n as u8),
            blur_strictness: number("blur_strictness", 10, 100)?.map(|n| n as u8),
            blur_img_size: number("blur_img_size", 32, 4096)?.map(|n| n as u16),
            blur_video_size: number("blur_video_size", 32, 4096)?.map(|n| n as u16),
            blur_min_size: number("blur_min_size", 1, 4096)?.map(|n| n as u16),
            // Same rule as the numbers: absent leaves it alone, present has to
            // name a model that exists. A misspelt name quietly falling back to
            // the default would look like the model being ignored.
            blur_model: match v.get("blur_model") {
                None | Some(serde_json::Value::Null) => None,
                Some(m) => Some(
                    m.as_str()
                        .and_then(BlurModel::from_id)
                        .ok_or_else(|| format!("blur_model must be one of: {}", BlurModel::names()))?,
                ),
            },
        })
    }
}

#[derive(Clone, Copy, Debug, Serialize)]
pub struct DecisionSettings {
    /// Whether Adblock does anything at all. Off, every request passes.
    pub enabled: bool,
    pub redirect: bool,
    pub removeparam: bool,
    pub csp: bool,
    pub cosmetic: bool,
    pub scriptlets: bool,
    pub runtime: bool,
    /// Find the people in pictures and blur them. Off by default: it downloads
    /// several megabytes of model on the first page that needs it.
    pub blur: bool,
    /// Blur the people the detector reads as men.
    pub blur_men: bool,
    /// Blur the people the detector reads as women.
    pub blur_women: bool,
    /// Look at still images at all. Off, no image is ever sent to the detector.
    pub blur_images: bool,
    /// Look at videos at all. Off, no video is. On, a video is sampled frame by
    /// frame while it plays, which costs far more than an image does.
    pub blur_videos: bool,
    /// Cover each person found, instead of blurring the whole picture.
    pub blur_regions: bool,
    /// Drain the colour out of what is blurred, as well as blurring it.
    pub blur_gray: bool,
    /// Blur every image and video from the moment it is seen, and lift the blur
    /// when the detector has nothing to hide there. Nothing is briefly readable
    /// before the verdict lands; the cost is a page that starts out obscured.
    pub blur_on_load: bool,
    /// Lift the blur off an image while the pointer is over it.
    pub blur_hover_images: bool,
    /// The same for a video.
    pub blur_hover_videos: bool,
    /// Outline every picture with what the detector made of it, and say so in
    /// its tooltip. For seeing that the thing runs at all.
    pub blur_marks: bool,
    /// Shrink a picture to fit the sizes below before the detector sees it. Off,
    /// every picture goes through at its own size: slower, and the detector may
    /// do better or worse on it.
    pub blur_resize: bool,
    /// Skip a picture too small to hold a face worth hiding. Icons, avatars,
    /// spacers and tracking pixels are most of what a page carries, and each one
    /// costs a run of the detector.
    pub blur_skip_small: bool,
    pub blur_amount: u8,
    pub blur_strictness: u8,
    /// Longest side an image is shrunk to. Bigger finds smaller faces and costs
    /// more time. Ignored while `blur_resize` is off.
    pub blur_img_size: u16,
    /// The same for a video frame.
    pub blur_video_size: u16,
    /// The size that counts as too small: a picture with either side under this
    /// many of its own pixels is skipped. Ignored while `blur_skip_small` is off.
    pub blur_min_size: u16,
    /// Which detector the runtime loads.
    pub blur_model: BlurModel,
}

impl DecisionSettings {
    /// Whether anything at all would go into a page. Nothing on means Adblock
    /// has no reason to read a response body.
    pub(crate) fn injects(&self) -> bool {
        self.cosmetic || self.scriptlets || self.runtime || self.blurring()
    }

    /// Whether the blur would do anything. With neither men nor women picked
    /// there is nobody to look for, and with neither images nor videos picked
    /// there is nothing to look at — so the script and the response header it
    /// needs are both pointless.
    pub(crate) fn blurring(&self) -> bool {
        self.blur && (self.blur_men || self.blur_women) && (self.blur_images || self.blur_videos)
    }
}

pub struct DecisionPolicy {
    enabled: AtomicBool,
    redirect: AtomicBool,
    removeparam: AtomicBool,
    csp: AtomicBool,
    cosmetic: AtomicBool,
    scriptlets: AtomicBool,
    runtime: AtomicBool,
    blur: AtomicBool,
    blur_men: AtomicBool,
    blur_women: AtomicBool,
    blur_images: AtomicBool,
    blur_videos: AtomicBool,
    blur_regions: AtomicBool,
    blur_gray: AtomicBool,
    blur_on_load: AtomicBool,
    blur_hover_images: AtomicBool,
    blur_hover_videos: AtomicBool,
    blur_marks: AtomicBool,
    blur_resize: AtomicBool,
    blur_skip_small: AtomicBool,
    blur_amount: AtomicU8,
    blur_strictness: AtomicU8,
    blur_img_size: AtomicU16,
    blur_video_size: AtomicU16,
    blur_min_size: AtomicU16,
    /// Held as the model's position in `BlurModel::ALL`, because that is what an
    /// atomic can carry. Nothing outside this file sees the number.
    blur_model: AtomicU8,
    path: PathBuf,
}

impl DecisionPolicy {
    /// Load the switches, all on by default. An empty path means an in-memory
    /// policy that is never written.
    pub fn load(path: PathBuf) -> Self {
        let saved: DecisionOverrides = std::fs::read_to_string(&path)
            .ok()
            .and_then(|t| serde_json::from_str(&t).ok())
            .unwrap_or_default();
        let policy = Self {
            enabled: AtomicBool::new(saved.enabled.unwrap_or(true)),
            redirect: AtomicBool::new(saved.redirect.unwrap_or(true)),
            removeparam: AtomicBool::new(saved.removeparam.unwrap_or(true)),
            csp: AtomicBool::new(saved.csp.unwrap_or(true)),
            cosmetic: AtomicBool::new(saved.cosmetic.unwrap_or(true)),
            scriptlets: AtomicBool::new(saved.scriptlets.unwrap_or(true)),
            runtime: AtomicBool::new(saved.runtime.unwrap_or(true)),
            // Every default from here to the strictness is HaramBlur's own,
            // except the master switch: ours downloads a model on the first page
            // that needs it, so it is opt-in.
            blur: AtomicBool::new(saved.blur.unwrap_or(false)),
            blur_men: AtomicBool::new(saved.blur_men.unwrap_or(false)),
            blur_women: AtomicBool::new(saved.blur_women.unwrap_or(true)),
            blur_images: AtomicBool::new(saved.blur_images.unwrap_or(true)),
            blur_videos: AtomicBool::new(saved.blur_videos.unwrap_or(true)),
            blur_regions: AtomicBool::new(saved.blur_regions.unwrap_or(true)),
            blur_gray: AtomicBool::new(saved.blur_gray.unwrap_or(true)),
            blur_on_load: AtomicBool::new(saved.blur_on_load.unwrap_or(false)),
            blur_hover_images: AtomicBool::new(saved.blur_hover_images.unwrap_or(false)),
            blur_hover_videos: AtomicBool::new(saved.blur_hover_videos.unwrap_or(false)),
            blur_marks: AtomicBool::new(saved.blur_marks.unwrap_or(false)),
            blur_resize: AtomicBool::new(saved.blur_resize.unwrap_or(true)),
            blur_skip_small: AtomicBool::new(saved.blur_skip_small.unwrap_or(true)),
            blur_amount: AtomicU8::new(saved.blur_amount.unwrap_or(25).clamp(10, 50)),
            blur_strictness: AtomicU8::new(saved.blur_strictness.unwrap_or(40).clamp(10, 100)),
            // The sizes HaramBlur feeds its detector, as a longest side.
            blur_img_size: AtomicU16::new(saved.blur_img_size.unwrap_or(400).clamp(32, 4096)),
            blur_video_size: AtomicU16::new(saved.blur_video_size.unwrap_or(427).clamp(32, 4096)),
            // Under 32 pixels a side there is no face for the detector to find,
            // only a spacer, a bullet or a tracking pixel.
            blur_min_size: AtomicU16::new(saved.blur_min_size.unwrap_or(32).clamp(1, 4096)),
            blur_model: AtomicU8::new(index_of(saved.blur_model.unwrap_or(BlurModel::Human))),
            path,
        };
        // Seed the file a fresh install does not have yet, so every switch is
        // there to edit by hand. Failing to seed it is not worth a warning: the
        // defaults still apply and saving from the UI will report the problem.
        if !policy.path.as_os_str().is_empty() && !policy.path.exists() {
            if let Err(e) = policy.persist() {
                tracing::debug!(error = %e, "seeding adblock settings file");
            }
        }
        policy
    }

    /// Every switch on, with no settings file behind them.
    pub fn all_on() -> Self {
        Self::load(PathBuf::new())
    }

    pub fn settings(&self) -> DecisionSettings {
        DecisionSettings {
            enabled: self.enabled.load(Ordering::Relaxed),
            redirect: self.redirect.load(Ordering::Relaxed),
            removeparam: self.removeparam.load(Ordering::Relaxed),
            csp: self.csp.load(Ordering::Relaxed),
            cosmetic: self.cosmetic.load(Ordering::Relaxed),
            scriptlets: self.scriptlets.load(Ordering::Relaxed),
            runtime: self.runtime.load(Ordering::Relaxed),
            blur: self.blur.load(Ordering::Relaxed),
            blur_men: self.blur_men.load(Ordering::Relaxed),
            blur_women: self.blur_women.load(Ordering::Relaxed),
            blur_images: self.blur_images.load(Ordering::Relaxed),
            blur_videos: self.blur_videos.load(Ordering::Relaxed),
            blur_regions: self.blur_regions.load(Ordering::Relaxed),
            blur_gray: self.blur_gray.load(Ordering::Relaxed),
            blur_on_load: self.blur_on_load.load(Ordering::Relaxed),
            blur_hover_images: self.blur_hover_images.load(Ordering::Relaxed),
            blur_hover_videos: self.blur_hover_videos.load(Ordering::Relaxed),
            blur_marks: self.blur_marks.load(Ordering::Relaxed),
            blur_resize: self.blur_resize.load(Ordering::Relaxed),
            blur_skip_small: self.blur_skip_small.load(Ordering::Relaxed),
            blur_amount: self.blur_amount.load(Ordering::Relaxed),
            blur_strictness: self.blur_strictness.load(Ordering::Relaxed),
            blur_img_size: self.blur_img_size.load(Ordering::Relaxed),
            blur_video_size: self.blur_video_size.load(Ordering::Relaxed),
            blur_min_size: self.blur_min_size.load(Ordering::Relaxed),
            blur_model: BlurModel::ALL[self.blur_model.load(Ordering::Relaxed) as usize],
        }
    }

    /// Apply an update and persist it. Takes effect on the next request — the
    /// switches are read per decision and per page, nothing is rebuilt.
    pub fn apply(&self, upd: &DecisionOverrides) -> DecisionSettings {
        for (flag, cell) in [
            (upd.enabled, &self.enabled),
            (upd.redirect, &self.redirect),
            (upd.removeparam, &self.removeparam),
            (upd.csp, &self.csp),
            (upd.cosmetic, &self.cosmetic),
            (upd.scriptlets, &self.scriptlets),
            (upd.runtime, &self.runtime),
            (upd.blur, &self.blur),
            (upd.blur_men, &self.blur_men),
            (upd.blur_women, &self.blur_women),
            (upd.blur_images, &self.blur_images),
            (upd.blur_videos, &self.blur_videos),
            (upd.blur_regions, &self.blur_regions),
            (upd.blur_gray, &self.blur_gray),
            (upd.blur_on_load, &self.blur_on_load),
            (upd.blur_hover_images, &self.blur_hover_images),
            (upd.blur_hover_videos, &self.blur_hover_videos),
            (upd.blur_marks, &self.blur_marks),
            (upd.blur_resize, &self.blur_resize),
            (upd.blur_skip_small, &self.blur_skip_small),
        ] {
            if let Some(v) = flag {
                cell.store(v, Ordering::Relaxed);
            }
        }
        for (num, cell) in
            [(upd.blur_amount, &self.blur_amount), (upd.blur_strictness, &self.blur_strictness)]
        {
            if let Some(v) = num {
                cell.store(v, Ordering::Relaxed);
            }
        }
        for (num, cell) in [
            (upd.blur_img_size, &self.blur_img_size),
            (upd.blur_video_size, &self.blur_video_size),
            (upd.blur_min_size, &self.blur_min_size),
        ] {
            if let Some(v) = num {
                cell.store(v, Ordering::Relaxed);
            }
        }
        if let Some(m) = upd.blur_model {
            self.blur_model.store(index_of(m), Ordering::Relaxed);
        }
        if let Err(e) = self.persist() {
            tracing::warn!(error = %e, "persisting adblock settings");
        }
        self.settings()
    }

    fn persist(&self) -> std::result::Result<(), String> {
        if self.path.as_os_str().is_empty() {
            return Ok(());
        }
        let snap = self.settings();
        write(
            &self.path,
            &DecisionOverrides {
                enabled: Some(snap.enabled),
                redirect: Some(snap.redirect),
                removeparam: Some(snap.removeparam),
                csp: Some(snap.csp),
                cosmetic: Some(snap.cosmetic),
                scriptlets: Some(snap.scriptlets),
                runtime: Some(snap.runtime),
                blur: Some(snap.blur),
                blur_men: Some(snap.blur_men),
                blur_women: Some(snap.blur_women),
                blur_images: Some(snap.blur_images),
                blur_videos: Some(snap.blur_videos),
                blur_regions: Some(snap.blur_regions),
                blur_gray: Some(snap.blur_gray),
                blur_on_load: Some(snap.blur_on_load),
                blur_hover_images: Some(snap.blur_hover_images),
                blur_hover_videos: Some(snap.blur_hover_videos),
                blur_marks: Some(snap.blur_marks),
                blur_resize: Some(snap.blur_resize),
                blur_skip_small: Some(snap.blur_skip_small),
                blur_amount: Some(snap.blur_amount),
                blur_strictness: Some(snap.blur_strictness),
                blur_img_size: Some(snap.blur_img_size),
                blur_video_size: Some(snap.blur_video_size),
                blur_min_size: Some(snap.blur_min_size),
                blur_model: Some(snap.blur_model),
            },
        )
    }
}

fn index_of(m: BlurModel) -> u8 {
    BlurModel::ALL.iter().position(|c| *c == m).unwrap_or(0) as u8
}

fn write(path: &Path, saved: &DecisionOverrides) -> std::result::Result<(), String> {
    if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
        std::fs::create_dir_all(dir).map_err(|e| format!("creating {}: {e}", dir.display()))?;
    }
    let body = serde_json::to_string_pretty(saved).map_err(|e| e.to_string())?;
    std::fs::write(path, body).map_err(|e| format!("writing {}: {e}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_master_switch_turns_off_and_survives_a_reload() {
        let path = std::env::temp_dir()
            .join(format!("adblock-enabled-test-{}", std::process::id()))
            .join("adblock.json");
        let _ = std::fs::remove_dir_all(path.parent().unwrap());

        let policy = DecisionPolicy::load(path.clone());
        assert!(policy.settings().enabled, "blocking is on by default");

        let off = DecisionOverrides { enabled: Some(false), ..Default::default() };
        assert!(!policy.apply(&off).enabled);
        assert!(policy.settings().redirect, "the other switches are left alone");
        assert!(!DecisionPolicy::load(path.clone()).settings().enabled, "and it is remembered");

        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn parse_picks_up_only_present_flags() {
        let o = DecisionOverrides::parse(br#"{"redirect":false,"cosmetic":false}"#).unwrap();
        assert_eq!(o.enabled, None, "the master switch is left alone too");
        assert_eq!(o.redirect, Some(false));
        assert_eq!(o.cosmetic, Some(false));
        assert_eq!(o.removeparam, None, "an absent key leaves that switch alone");
        assert_eq!(o.csp, None);
        assert_eq!(o.scriptlets, None);
        assert_eq!(o.runtime, None);
        assert!(DecisionOverrides::parse(b"[]").is_err(), "not an object");
        assert!(DecisionOverrides::parse(b"nonsense").is_err());
    }

    #[test]
    fn blur_numbers_are_rejected_rather_than_clamped() {
        let o =
            DecisionOverrides::parse(br#"{"blur":true,"blur_amount":10,"blur_strictness":100}"#)
                .unwrap();
        assert_eq!((o.blur, o.blur_amount, o.blur_strictness), (Some(true), Some(10), Some(100)));
        assert_eq!(DecisionOverrides::parse(b"{}").unwrap().blur_amount, None, "absent is fine");
        for bad in [
            // The radius and the strictness both run over HaramBlur's range, so
            // a number outside it is a caller sending something else's units.
            &br#"{"blur_amount":9}"#[..],
            &br#"{"blur_amount":51}"#[..],
            &br#"{"blur_strictness":9}"#[..],
            &br#"{"blur_strictness":101}"#[..],
            &br#"{"blur_amount":"20"}"#[..],
            &br#"{"blur_amount":-1}"#[..],
            &br#"{"blur_img_size":16}"#[..],
            &br#"{"blur_video_size":5000}"#[..],
            &br#"{"blur_min_size":0}"#[..],
            &br#"{"blur_min_size":5000}"#[..],
        ] {
            assert!(DecisionOverrides::parse(bad).is_err(), "{}", String::from_utf8_lossy(bad));
        }
    }

    #[test]
    fn blur_is_off_by_default_and_pulls_in_a_page_edit_when_turned_on() {
        let policy = DecisionPolicy::all_on();
        let s = policy.settings();
        assert!(!s.blur, "the model download is opt-in");
        assert!(!s.blur_marks, "the outlines are a debugging aid, not the normal look");
        // Everything else here is HaramBlur's own default, so that switching the
        // master switch on gives what HaramBlur gives out of the box.
        assert!(s.blur_regions, "a patch per person");
        assert!(!s.blur_men && s.blur_women, "women only");
        assert!(s.blur_images && s.blur_videos, "both kinds of media");
        assert!(s.blur_gray, "the colour comes out with the blur");
        assert!(!s.blur_on_load, "nothing is blurred before it has been looked at");
        assert!(!s.blur_hover_images && !s.blur_hover_videos, "hover does not lift it");
        assert_eq!((s.blur_amount, s.blur_strictness), (25, 40));
        assert!(s.blur_resize, "shrinking first is the normal path");
        assert_eq!((s.blur_img_size, s.blur_video_size), (400, 427));
        assert!(s.blur_skip_small, "an icon cannot hold a face and costs a run of the detector");
        assert_eq!(s.blur_min_size, 32);

        let snap = policy.apply(&DecisionOverrides {
            cosmetic: Some(false),
            scriptlets: Some(false),
            runtime: Some(false),
            blur: Some(true),
            blur_amount: Some(40),
            ..Default::default()
        });
        assert!(snap.injects(), "blur alone is still a reason to read the page");
        assert_eq!(snap.blur_amount, 40);
        assert_eq!(snap.blur_strictness, 40, "an untouched number stays");

        let snap = policy.apply(&DecisionOverrides {
            blur_men: Some(false),
            blur_women: Some(false),
            ..Default::default()
        });
        assert!(snap.blur, "the switch is still on");
        assert!(!snap.blurring(), "but there is nobody left to look for");
        assert!(!snap.injects(), "so nothing goes into the page");
    }

    #[test]
    fn apply_persists_and_reloads() {
        let path = std::env::temp_dir()
            .join(format!("adblock-settings-test-{}", std::process::id()))
            .join("adblock.json");
        let _ = std::fs::remove_dir_all(path.parent().unwrap());

        let policy = DecisionPolicy::load(path.clone());
        let s = policy.settings();
        assert_eq!(
            (s.redirect, s.removeparam, s.csp, s.cosmetic, s.scriptlets, s.runtime),
            (true, true, true, true, true, true),
            "defaults are on"
        );
        assert!(path.exists(), "a fresh install gets a settings file to edit");

        let snap = policy.apply(&DecisionOverrides {
            redirect: Some(false),
            csp: Some(false),
            runtime: Some(false),
            ..Default::default()
        });
        assert_eq!(
            (snap.redirect, snap.removeparam, snap.csp, snap.cosmetic, snap.runtime),
            (false, true, false, true, false),
            "untouched switch stays"
        );
        assert!(snap.injects(), "cosmetic and scriptlets are still on");

        let reloaded = DecisionPolicy::load(path.clone()).settings();
        assert_eq!(
            (reloaded.redirect, reloaded.removeparam, reloaded.csp, reloaded.runtime),
            (false, true, false, false)
        );
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn nothing_to_inject_when_every_page_switch_is_off() {
        let policy = DecisionPolicy::all_on();
        assert!(policy.settings().injects());
        let snap = policy.apply(&DecisionOverrides {
            cosmetic: Some(false),
            scriptlets: Some(false),
            runtime: Some(false),
            ..Default::default()
        });
        assert!(!snap.injects(), "no page edits left to make");
    }
}
