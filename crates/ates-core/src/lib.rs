//! Pure compute for the ATES estimator: raster grids, CRS helpers, and
//! terrain derivatives. This crate performs no I/O.

pub mod autoates;
pub mod classify;
pub mod crs;
pub mod flowpy;
pub mod grid;
pub mod overhead;
pub mod pra;
pub mod sieve;
pub mod terrain;

pub use crs::{BBox, Crs};
pub use grid::{GeoTransform, Grid, GridError};
