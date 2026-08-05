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
//! the colour out along with the blur, a switch for which way media is held back
//! until a verdict arrives, a switch each for lifting the blur while the pointer
//! is over an image and over a video, how hard to blur and how sure the detector
//! has to be.
//!
//! One switch here is ours rather than HaramBlur's: whether to show what the
//! detector did. There is no setting for which detector to load, because there is
//! — HaramBlur's own. Comparing it against the other pipeline is done by
//! switching branches, not by a picker.
//!
//! All of them belong to Adblock rather than the caller. The caller hands over a
//! request or a response and takes back what Adblock made of it; it never asks
//! for a rule to be applied, so it has nothing to switch off.
//!
//! Persisted to Adblock's own settings file. Nothing else writes that file, so
//! saving rewrites it whole.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};

use serde::{Deserialize, Serialize};

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
    /// Blur radius in CSS pixels, 10–50 — HaramBlur's own range.
    pub blur_amount: Option<u8>,
    /// How eager the detector is, 10–100 — HaramBlur's 0.1–1 as a percentage.
    /// Higher blurs on weaker evidence.
    pub blur_strictness: Option<u8>,
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
            blur_amount: number("blur_amount", 10, 50)?.map(|n| n as u8),
            blur_strictness: number("blur_strictness", 10, 100)?.map(|n| n as u8),
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
    /// Blur the people the detector reads as women or as girls. The detector
    /// splits those two by how old the body looks, which is not what this
    /// switch is asking about, so both go with it.
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
    /// Which way a picture is held back until the detector has looked at it.
    /// Nothing is ever shown before a verdict either way, however long that
    /// takes; this picks blurred over hidden. HaramBlur's `blurryStartMode`.
    pub blur_on_load: bool,
    /// Lift the blur off an image while the pointer is over it.
    pub blur_hover_images: bool,
    /// The same for a video.
    pub blur_hover_videos: bool,
    /// Outline every picture with what the detector made of it, and say so in
    /// its tooltip. For seeing that the thing runs at all.
    pub blur_marks: bool,
    pub blur_amount: u8,
    pub blur_strictness: u8,
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
    blur_amount: AtomicU8,
    blur_strictness: AtomicU8,
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
            blur_amount: AtomicU8::new(saved.blur_amount.unwrap_or(25).clamp(10, 50)),
            blur_strictness: AtomicU8::new(saved.blur_strictness.unwrap_or(40).clamp(10, 100)),
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
            blur_amount: self.blur_amount.load(Ordering::Relaxed),
            blur_strictness: self.blur_strictness.load(Ordering::Relaxed),
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
                blur_amount: Some(snap.blur_amount),
                blur_strictness: Some(snap.blur_strictness),
            },
        )
    }
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
