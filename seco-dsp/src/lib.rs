//! SECO DSP utilities: allocation-free, host-agnostic, unit-testable.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

mod curve;
pub mod duck;
mod slew;
mod smooth;

pub use curve::CurveTable;
pub use duck::DuckShape;
pub use slew::Slew;
pub use smooth::OnePole;
