//! Dalia-inspired geometry recipes.
//!
//! These presets are data, not a dependency on a browser/WebGL runtime. A
//! native Dalia renderer can consume them directly; a compatibility renderer
//! can approximate them with the same palette and movement parameters.

use crate::Color;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Motif {
    Orbit,
    Lissajous,
    Lattice,
    Spiral,
    Pulse,
    Ribbon,
    Shards,
    Tunnel,
    Wave,
    Bloom,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct DaliaPreset {
    pub id: &'static str,
    pub label: &'static str,
    pub motif: Motif,
    pub base: Color,
    pub accent: Color,
    /// How much loudness affects size, in normalized units.
    pub loudness_gain: f32,
    /// How much stereo width bends the form.
    pub width_gain: f32,
    /// How much spectral centroid spins the form.
    pub spectral_gain: f32,
}

const COBALT: Color = Color::rgb(0.102, 0.400, 0.980);
const GUAVA: Color = Color::rgb(1.000, 0.270, 0.204);
const VIOLET: Color = Color::rgb(0.560, 0.310, 0.910);
const NICKEL: Color = Color::rgb(0.710, 0.780, 0.784);
const MANGO: Color = Color::rgb(1.000, 0.690, 0.160);

/// Twenty named, deterministic starting points. IDs stay stable: saved
/// sessions should reference an intent, not a position in a menu.
pub const PRESETS: [DaliaPreset; 20] = [
    preset(
        "neta-linea",
        "Línea",
        Motif::Wave,
        COBALT,
        NICKEL,
        0.24,
        0.10,
        0.15,
    ),
    preset(
        "neta-azotea",
        "Azotea",
        Motif::Lattice,
        COBALT,
        GUAVA,
        0.20,
        0.23,
        0.11,
    ),
    preset(
        "neta-luciernaga",
        "Luciérnaga",
        Motif::Orbit,
        MANGO,
        GUAVA,
        0.33,
        0.15,
        0.28,
    ),
    preset(
        "neta-telefono",
        "Teléfono",
        Motif::Pulse,
        VIOLET,
        NICKEL,
        0.39,
        0.08,
        0.18,
    ),
    preset(
        "neta-marea",
        "Marea",
        Motif::Ribbon,
        COBALT,
        VIOLET,
        0.26,
        0.30,
        0.14,
    ),
    preset(
        "neta-barrio",
        "Barrio",
        Motif::Shards,
        GUAVA,
        MANGO,
        0.31,
        0.34,
        0.22,
    ),
    preset(
        "neta-cenit",
        "Cénit",
        Motif::Tunnel,
        NICKEL,
        COBALT,
        0.19,
        0.12,
        0.44,
    ),
    preset(
        "neta-bruja",
        "Bruja",
        Motif::Lissajous,
        VIOLET,
        GUAVA,
        0.27,
        0.38,
        0.29,
    ),
    preset(
        "neta-petalo",
        "Pétalo",
        Motif::Bloom,
        GUAVA,
        NICKEL,
        0.41,
        0.16,
        0.12,
    ),
    preset(
        "neta-eco",
        "Eco",
        Motif::Spiral,
        COBALT,
        MANGO,
        0.16,
        0.18,
        0.41,
    ),
    preset(
        "neta-tiniebla",
        "Tiniebla",
        Motif::Tunnel,
        VIOLET,
        COBALT,
        0.22,
        0.32,
        0.36,
    ),
    preset(
        "neta-cromo",
        "Cromo",
        Motif::Lattice,
        NICKEL,
        VIOLET,
        0.17,
        0.29,
        0.20,
    ),
    preset(
        "neta-temblor",
        "Temblor",
        Motif::Shards,
        GUAVA,
        COBALT,
        0.46,
        0.42,
        0.09,
    ),
    preset(
        "neta-limbo",
        "Limbo",
        Motif::Orbit,
        VIOLET,
        NICKEL,
        0.12,
        0.41,
        0.31,
    ),
    preset(
        "neta-volador",
        "Volador",
        Motif::Ribbon,
        MANGO,
        COBALT,
        0.29,
        0.35,
        0.25,
    ),
    preset(
        "neta-peso",
        "Peso",
        Motif::Pulse,
        GUAVA,
        VIOLET,
        0.50,
        0.06,
        0.07,
    ),
    preset(
        "neta-trompo",
        "Trompo",
        Motif::Spiral,
        MANGO,
        VIOLET,
        0.21,
        0.27,
        0.50,
    ),
    preset(
        "neta-sombra",
        "Sombra",
        Motif::Wave,
        NICKEL,
        COBALT,
        0.14,
        0.19,
        0.23,
    ),
    preset(
        "neta-zocalo",
        "Zócalo",
        Motif::Lissajous,
        GUAVA,
        MANGO,
        0.35,
        0.39,
        0.18,
    ),
    preset(
        "neta-satelite",
        "Satélite",
        Motif::Bloom,
        COBALT,
        VIOLET,
        0.28,
        0.26,
        0.39,
    ),
];

// Table constructor mirrors every persisted preset field. Splitting its
// values into temporary structs would obscure the fixed data more than it
// would improve this private initializer.
#[allow(clippy::too_many_arguments)]
const fn preset(
    id: &'static str,
    label: &'static str,
    motif: Motif,
    base: Color,
    accent: Color,
    loudness_gain: f32,
    width_gain: f32,
    spectral_gain: f32,
) -> DaliaPreset {
    DaliaPreset {
        id,
        label,
        motif,
        base,
        accent,
        loudness_gain,
        width_gain,
        spectral_gain,
    }
}

pub fn by_id(id: &str) -> Option<&'static DaliaPreset> {
    PRESETS.iter().find(|preset| preset.id == id)
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ReactiveTransform {
    pub scale: f32,
    pub bend: f32,
    pub rotation_radians: f32,
    pub colour: Color,
}

impl DaliaPreset {
    /// Derives render-only motion from normalized analysis values. Inputs are
    /// clamped, so a dropped or malformed frame cannot teleport an object.
    pub fn react(
        self,
        loudness: f32,
        stereo_width: f32,
        spectral_centroid: f32,
    ) -> ReactiveTransform {
        let loudness = finite_unit(loudness);
        let stereo_width = finite_unit(stereo_width);
        let spectral_centroid = finite_unit(spectral_centroid);
        ReactiveTransform {
            scale: 1.0 + loudness * self.loudness_gain,
            bend: (stereo_width - 0.5) * 2.0 * self.width_gain,
            rotation_radians: spectral_centroid * std::f32::consts::TAU * self.spectral_gain,
            colour: self
                .base
                .mix(self.accent, loudness * 0.65 + spectral_centroid * 0.35),
        }
    }
}

fn finite_unit(value: f32) -> f32 {
    if value.is_finite() {
        value.clamp(0.0, 1.0)
    } else {
        0.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn presets_have_stable_unique_ids() {
        assert_eq!(PRESETS.len(), 20);
        for (index, preset) in PRESETS.iter().enumerate() {
            assert_eq!(by_id(preset.id), Some(preset));
            assert!(
                PRESETS[index + 1..]
                    .iter()
                    .all(|later| later.id != preset.id)
            );
        }
    }

    #[test]
    fn reactive_values_stay_bounded_for_bad_input() {
        let value = PRESETS[0].react(-99.0, f32::NAN, 99.0);
        assert!(value.scale.is_finite());
        assert!(value.bend.is_finite());
        assert!(value.rotation_radians.is_finite());
    }
}
