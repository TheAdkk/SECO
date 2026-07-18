/// Musical time information for one audio block, as reported by the host.
///
/// Hosts declare validity per field group (CLAP does it with flags); anything
/// the host did not provide is `None`. Values are plain musical units — the
/// adapter converts from whatever encoding the ABI uses.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Transport {
    /// Tempo in beats per minute, if the host provides one.
    pub tempo_bpm: Option<f64>,
    /// Song position in beats, if the host has a beats timeline.
    ///
    /// What one "beat" means (quarter note vs. time-signature denominator)
    /// is not defined by the CLAP header and may be host-dependent; SECO
    /// passes through what the host reports.
    pub song_pos_beats: Option<f64>,
    /// Song position in seconds, if the host has a seconds timeline.
    pub song_pos_seconds: Option<f64>,
    /// Time signature as `(numerator, denominator)`, if provided.
    pub time_signature: Option<(u16, u16)>,
    /// Whether the host transport is rolling.
    pub playing: bool,
}
