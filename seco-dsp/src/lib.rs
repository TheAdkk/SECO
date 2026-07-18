//! SECO DSP utilities: allocation-free, host-agnostic, unit-testable.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

mod curve;
mod smooth;

pub use curve::CurveTable;
pub use smooth::OnePole;
