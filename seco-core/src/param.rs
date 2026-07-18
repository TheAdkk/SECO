/// Describes one plugin parameter.
///
/// A plugin declares its parameters as a `const` slice
/// ([`Plugin::PARAMS`](crate::Plugin::PARAMS)). The parameter's *identifier*
/// is its index in that slice, so the order is append-only: reordering or
/// removing entries breaks saved sessions.
///
/// Parameter *values* are owned by the adapter, not the plugin: hosts read
/// them from the main thread while audio runs, so they live in atomics on
/// the adapter side and reach `process()` as a per-block snapshot
/// ([`RtContext::param`](crate::RtContext::param)).
#[derive(Clone, Copy, Debug)]
pub struct ParamDesc {
    /// Display name, e.g. `"Mix"`.
    pub name: &'static str,
    /// Value range and interpretation.
    pub range: ParamRange,
}

/// The value model of a parameter. Plain values are `f64` everywhere; the
/// range says how to interpret and display them.
#[derive(Clone, Copy, Debug)]
pub enum ParamRange {
    /// A continuous value in `[min, max]`.
    Continuous {
        /// Lower bound (finite).
        min: f64,
        /// Upper bound (finite).
        max: f64,
        /// Initial value, within `[min, max]`.
        default: f64,
    },
    /// An enumerated value: plain value `n` means `labels[n]`. Every label
    /// must be non-empty (hosts render them).
    Stepped {
        /// One label per step, in value order.
        labels: &'static [&'static str],
        /// Initial step index.
        default: usize,
    },
    /// An on/off switch: plain value `0.0` or `1.0`.
    Toggle {
        /// Initial state.
        default: bool,
        /// Marks this as *the* bypass parameter, merged with the host's
        /// bypass button. At most one per plugin. Bypass must not stop the
        /// host from calling `process()` — implement it as a passthrough
        /// inside the plugin.
        bypass: bool,
    },
}

impl ParamRange {
    /// The minimum plain value.
    pub fn min(&self) -> f64 {
        match self {
            Self::Continuous { min, .. } => *min,
            Self::Stepped { .. } | Self::Toggle { .. } => 0.0,
        }
    }

    /// The maximum plain value.
    pub fn max(&self) -> f64 {
        match self {
            Self::Continuous { max, .. } => *max,
            Self::Stepped { labels, .. } => labels.len().saturating_sub(1) as f64,
            Self::Toggle { .. } => 1.0,
        }
    }

    /// The default plain value.
    pub fn default_plain(&self) -> f64 {
        match self {
            Self::Continuous { default, .. } => *default,
            Self::Stepped { default, .. } => *default as f64,
            Self::Toggle { default, .. } => f64::from(u8::from(*default)),
        }
    }
}
