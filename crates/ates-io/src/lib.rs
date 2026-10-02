//! Raster I/O for the ATES estimator.
//!
//! Sources and sinks sit behind the [`RasterSource`] and [`RasterSink`]
//! traits so the GDAL backend (feature `gdal`, on by default) can later be
//! joined by a pure-Rust one.

pub mod provenance;
pub mod raster;
pub mod route_file;

#[cfg(feature = "gdal")]
pub mod gdal_backend;

pub use provenance::{DISCLAIMER, Provenance};
pub use raster::{
    Band, BandData, GridSource, IoError, Projector, RasterSink, RasterSource, Resampling,
    WindowRequest,
};
