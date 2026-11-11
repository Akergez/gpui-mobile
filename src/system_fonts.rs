//! What the system says of its text: the font it draws with, how large and
//! how heavy the person asked text to be.
//!
//! On Android the Java side (`dev.gpui.mobile.GpuiFonts.describe`) answers in
//! lines of tab-separated fields (a tab is `→` here), so that this half can
//! be tested anywhere:
//!
//! ```text
//! scale → 1.15
//! weight → 300
//! face → /product/fonts/GoogleSansText-Regular.ttf → 0
//! face → /product/fonts/GoogleSansText-Bold.ttf → 0
//! ```
//!
//! `scale` is how much larger than a dp the system draws text (the font size
//! setting, through Android 14's non-linear curve at body size), `weight` what
//! the "bold text" setting adds to every weight, and each `face` a file (and
//! its index in a collection) the system draws its theme's text with; the
//! first is the regular one. Lines it does not know are skipped, so either
//! half can grow.

/// One file of the system's font.
#[derive(Debug, Clone, PartialEq)]
pub struct Face {
    pub path: String,
    pub index: u32,
}

/// The system's text, as [`parse`] reads it.
#[derive(Debug, Clone, PartialEq)]
pub struct SystemFonts {
    /// What text is drawn at, for each logical pixel of the rest: 1.0 when
    /// the person has not changed the font size.
    pub text_scale: f32,
    /// What the system adds to every weight it is asked for (300 with
    /// "bold text" on, 0 without).
    pub weight_adjustment: f32,
    /// The files of the font the system draws its text with, the regular one
    /// first. Empty when it could not say.
    pub faces: Vec<Face>,
}

impl Default for SystemFonts {
    fn default() -> Self {
        Self {
            text_scale: 1.0,
            weight_adjustment: 0.0,
            faces: Vec::new(),
        }
    }
}

/// Reads the Java side's answer. What is missing or does not make sense is
/// left as it is by default: text at its size and weight, GPUI's font.
pub fn parse(answer: &str) -> SystemFonts {
    let mut fonts = SystemFonts::default();
    for line in answer.lines() {
        let mut fields = line.split('\t');
        match (fields.next(), fields.next()) {
            (Some("scale"), Some(value)) => match value.trim().parse::<f32>() {
                Ok(scale) if (0.5..=3.0).contains(&scale) => fonts.text_scale = scale,
                _ => {}
            },
            (Some("weight"), Some(value)) => match value.trim().parse::<f32>() {
                Ok(adjustment) if (-1000.0..=1000.0).contains(&adjustment) => {
                    fonts.weight_adjustment = adjustment
                }
                _ => {}
            },
            (Some("face"), Some(path)) if !path.is_empty() => {
                let index = fields
                    .next()
                    .and_then(|index| index.trim().parse().ok())
                    .unwrap_or(0);
                let face = Face {
                    path: path.to_string(),
                    index,
                };
                if !fonts.faces.contains(&face) {
                    fonts.faces.push(face);
                }
            }
            _ => {}
        }
    }
    fonts
}

/// The weight the system draws when `weight` is asked for: with "bold text"
/// on, each weight is heavier by the adjustment, within OpenType's 1–1000.
pub fn adjusted_weight(weight: f32, adjustment: f32) -> f32 {
    if adjustment == 0.0 {
        return weight;
    }
    (weight + adjustment).clamp(1.0, 1000.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_answer_is_read_whole() {
        let fonts = parse(
            "scale\t1.15\nweight\t300\nface\t/product/fonts/GoogleSansText-Regular.ttf\t0\n\
             face\t/system/fonts/NotoSansCJK-Regular.ttc\t2\n",
        );
        assert_eq!(fonts.text_scale, 1.15);
        assert_eq!(fonts.weight_adjustment, 300.0);
        assert_eq!(
            fonts.faces,
            vec![
                Face {
                    path: "/product/fonts/GoogleSansText-Regular.ttf".into(),
                    index: 0
                },
                Face {
                    path: "/system/fonts/NotoSansCJK-Regular.ttc".into(),
                    index: 2
                },
            ]
        );
    }

    #[test]
    fn nothing_said_is_text_as_it_is() {
        assert_eq!(parse(""), SystemFonts::default());
        assert_eq!(
            parse("scale\tbig\nweight\t\nface\t\nsomething\telse"),
            SystemFonts::default()
        );
    }

    #[test]
    fn a_scale_out_of_reason_is_not_taken() {
        assert_eq!(parse("scale\t0").text_scale, 1.0);
        assert_eq!(parse("scale\tNaN").text_scale, 1.0);
        assert_eq!(parse("scale\t2.0").text_scale, 2.0);
    }

    #[test]
    fn a_file_named_twice_is_one_face() {
        let fonts = parse("face\t/a.ttf\t0\nface\t/a.ttf\t0\nface\t/a.ttf");
        assert_eq!(fonts.faces.len(), 1);
    }

    #[test]
    fn bold_text_makes_every_weight_heavier_within_bounds() {
        assert_eq!(adjusted_weight(400.0, 0.0), 400.0);
        assert_eq!(adjusted_weight(400.0, 300.0), 700.0);
        assert_eq!(adjusted_weight(900.0, 300.0), 1000.0);
        assert_eq!(adjusted_weight(100.0, -200.0), 1.0);
    }
}
