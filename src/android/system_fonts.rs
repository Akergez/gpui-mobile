//! The system's text, asked of Java once a process (see
//! [`crate::system_fonts`] for what is asked and how it answers).
//!
//! Android starts the activity over when the font size or "bold text"
//! changes (neither is in the manifest's `configChanges`), and an
//! application on gpui-mobile is one run per process, so what is read at the
//! start holds for the whole run.

use std::sync::OnceLock;

use jni::objects::{JObject, JValue};

use super::jni as jni_helpers;
pub use crate::system_fonts::{Face, SystemFonts};

static SYSTEM_FONTS: OnceLock<SystemFonts> = OnceLock::new();

/// What the system says of its text; text as it is when it could not say
/// (no activity yet, an application without `dev.gpui.mobile.GpuiFonts`).
pub fn system_fonts() -> &'static SystemFonts {
    SYSTEM_FONTS.get_or_init(|| match ask() {
        Ok(answer) => {
            let fonts = crate::system_fonts::parse(&answer);
            log::info!(
                "system text: scale {}, weight {:+}, font {:?}",
                fonts.text_scale,
                fonts.weight_adjustment,
                fonts
                    .faces
                    .iter()
                    .map(|face| face.path.as_str())
                    .collect::<Vec<_>>()
            );
            fonts
        }
        Err(error) => {
            log::warn!("the system's text is not known ({error}); text as it is, in Roboto");
            SystemFonts::default()
        }
    })
}

/// How much larger than the rest the person wants text: the font size
/// setting. The window's scale is the display's density times this.
pub fn text_scale() -> f32 {
    system_fonts().text_scale
}

fn ask() -> Result<String, String> {
    jni_helpers::with_env(|env| {
        let activity = jni_helpers::activity(env)?;
        let class = jni_helpers::find_app_class(env, "dev.gpui.mobile.GpuiFonts")?;
        let answer = env
            .call_static_method(
                &class,
                jni::jni_str!("describe"),
                jni::jni_sig!("(Landroid/content/Context;)Ljava/lang/String;"),
                &[JValue::Object(&activity)],
            )
            .and_then(|value| value.l())
            .map_err(|error| {
                env.exception_clear();
                error.to_string()
            })?;
        if answer.is_null() {
            return Ok(String::new());
        }
        let answer: JObject = answer;
        Ok(jni_helpers::get_string(env, &answer))
    })
}
