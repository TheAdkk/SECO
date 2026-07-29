//! What the editor remembers between sessions.
//!
//! Which modules are on and which model is loaded are the user's choices,
//! not measurements, so they travel in the plugin's state block rather than
//! in parameters: a host has no business automating whether the waveform is
//! visible, and a preset that silently rearranged the window would be worse
//! than one that did not save the layout at all.
//!
//! The format is deliberately plain text. It is written by the page, read
//! back by the page, and never parsed on the audio thread — Neta's
//! `apply_state` ignores it, because the meter measures the same either way.

/// The modules the editor can show, in the order the state block stores
/// their flags. Appending is safe; reordering this array silently swaps a
/// user's layout, because the flags are positional.
pub(crate) const MODULES: [&str; 7] = [
    "loudness",
    "spectrum",
    "spectrogram",
    "waveform",
    "stereo",
    "object",
    "visuals",
];

/// Longest model path accepted from a state block, so a corrupt session
/// cannot make the page allocate without bound.
const MAX_PATH: usize = 4_096;

/// Column widths are relative weights. The bounds only have to keep a
/// column from vanishing or swallowing the window.
const MIN_WIDTH: u32 = 20;
const MAX_WIDTH: u32 = 2_000;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Settings {
    pub(crate) enabled: [bool; MODULES.len()],
    /// Left-to-right order of the columns, as indices into [`MODULES`].
    pub(crate) order: [usize; MODULES.len()],
    /// Relative column widths, in the same order as [`MODULES`] — not as
    /// `order`, so moving a column carries its width with it.
    pub(crate) widths: [u32; MODULES.len()],
    pub(crate) model: String,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            enabled: [true; MODULES.len()],
            // The object opens the row and the visual closes it, which is
            // the layout the window was designed around.
            order: [5, 0, 1, 2, 3, 4, 6],
            widths: [130, 150, 200, 190, 170, 130, 130],
            model: String::new(),
        }
    }
}

impl Settings {
    /// Reads a block. Anything unrecognised is left at its default rather
    /// than rejected: a session written by a newer build should still open.
    pub(crate) fn parse(block: &str) -> Self {
        let mut settings = Settings::default();
        for field in block.split(';') {
            let Some((key, value)) = field.split_once('=') else {
                continue;
            };
            match key.trim() {
                "m" => {
                    // Missing trailing flags keep their default, so a block
                    // from a build with fewer modules still opens.
                    for (slot, flag) in settings.enabled.iter_mut().zip(value.chars()) {
                        *slot = flag == '1';
                    }
                }
                "o" => {
                    // A partial or repeated order would drop or duplicate a
                    // column, so it is accepted only as a whole permutation.
                    let digits: Vec<usize> = value
                        .chars()
                        .filter_map(|digit| digit.to_digit(10))
                        .map(|digit| digit as usize)
                        .collect();
                    if digits.len() == MODULES.len()
                        && (0..MODULES.len()).all(|module| digits.contains(&module))
                        && let Ok(order) = <[usize; MODULES.len()]>::try_from(digits)
                    {
                        settings.order = order;
                    }
                }
                "w" => {
                    // Widths are relative, so the only real constraints are
                    // "one per module" and "not zero" — a zero would hide a
                    // column the user still thinks is enabled.
                    let numbers: Vec<u32> = value
                        .split(',')
                        .filter_map(|number| number.parse().ok())
                        .filter(|number| (MIN_WIDTH..=MAX_WIDTH).contains(number))
                        .collect();
                    if let Ok(widths) = <[u32; MODULES.len()]>::try_from(numbers) {
                        settings.widths = widths;
                    }
                }
                "model" if value.len() <= MAX_PATH && !value.contains(['\n', '\r']) => {
                    settings.model = value.to_owned();
                }
                _ => {}
            }
        }
        settings
    }

    /// The block as the JavaScript call that hands it to the page.
    pub(crate) fn to_script(&self) -> String {
        let flags: Vec<&str> = self
            .enabled
            .iter()
            .map(|on| if *on { "true" } else { "false" })
            .collect();
        let order: Vec<String> = self.order.iter().map(usize::to_string).collect();
        let widths: Vec<String> = self.widths.iter().map(u32::to_string).collect();
        format!(
            "window.__neta_settings&&window.__neta_settings([{}],[{}],[{}],{});",
            flags.join(","),
            order.join(","),
            widths.join(","),
            crate::model::quote(&self.model)
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The blocks here are written by hand in the page's format on purpose:
    /// the editor's `save()` is what produces them in the field, so testing
    /// against a Rust serializer would only prove Rust agrees with itself.
    #[test]
    fn a_block_from_the_page_round_trips() {
        assert_eq!(
            Settings::parse(
                "m=1011010;o=3140256;w=100,110,120,130,140,150,160;model=/Users/someone/car.obj"
            ),
            Settings {
                enabled: [true, false, true, true, false, true, false],
                order: [3, 1, 4, 0, 2, 5, 6],
                widths: [100, 110, 120, 130, 140, 150, 160],
                model: "/Users/someone/car.obj".to_owned(),
            }
        );
    }

    #[test]
    fn defaults_show_everything_and_load_nothing() {
        let settings = Settings::default();
        assert!(settings.enabled.iter().all(|on| *on));
        assert!(settings.model.is_empty());
        assert_eq!(Settings::parse(""), settings);
        assert_eq!(Settings::parse("nonsense"), settings);
    }

    /// A session written by a build with more or fewer modules must still
    /// open, because the alternative is a user losing their layout on every
    /// update.
    #[test]
    fn unknown_fields_and_short_flag_runs_are_tolerated() {
        let parsed = Settings::parse("m=01;theme=whatever;model=/tmp/a.obj");
        assert!(!parsed.enabled[0]);
        assert!(parsed.enabled[1]);
        // Beyond the supplied flags, defaults stand.
        assert!(parsed.enabled[2..].iter().all(|on| *on));
        assert_eq!(parsed.model, "/tmp/a.obj");

        let long = Settings::parse("m=1111111111111111");
        assert!(long.enabled.iter().all(|on| *on));
    }

    /// A dropped or duplicated column is worse than an unsaved layout: the
    /// module simply vanishes from the window with no way to bring it back.
    #[test]
    fn a_broken_order_falls_back_rather_than_losing_a_column() {
        let default = Settings::default().order;
        for broken in ["o=012345", "o=01234567", "o=0112345", "o=abcdefg", "o=", "o=9999999"] {
            assert_eq!(
                Settings::parse(broken).order,
                default,
                "{broken} should not have been accepted"
            );
        }
        assert_eq!(Settings::parse("o=6543210").order, [6, 5, 4, 3, 2, 1, 0]);
    }

    /// Same reasoning as the order: a zero-width or missing column is a
    /// module the user cannot get back without resetting everything.
    #[test]
    fn broken_widths_fall_back_rather_than_collapsing_a_column() {
        let default = Settings::default().widths;
        for broken in [
            "w=100,100,100",
            "w=0,100,100,100,100,100,100",
            "w=100,100,100,100,100,100,99999",
            "w=",
            "w=a,b,c,d,e,f,g",
        ] {
            assert_eq!(
                Settings::parse(broken).widths,
                default,
                "{broken} should not have been accepted"
            );
        }
        assert_eq!(
            Settings::parse("w=20,30,40,50,60,70,80").widths,
            [20, 30, 40, 50, 60, 70, 80]
        );
    }

    #[test]
    fn an_absurd_path_is_refused() {
        let block = format!("model={}", "a".repeat(MAX_PATH + 1));
        assert!(Settings::parse(&block).model.is_empty());
    }
}
