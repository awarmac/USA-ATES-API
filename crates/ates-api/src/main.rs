//! `ates-api`: serve region builds over HTTP.
//!
//! ```text
//! ates-api --data-dir data/regions --bind 127.0.0.1:8080
//! ates-api ... --caic-products products.json --caic-areas areas.geojson --treeline 3300,3500
//! ```

use std::net::SocketAddr;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;

use ates_api::regions::load_regions;
use ates_api::{AppState, ForecastState, router};
use ates_forecast::{CaicFiles, ForecastProvider, Treeline};
use clap::Parser;

#[derive(Parser)]
#[command(
    name = "ates-api",
    version,
    about = "HTTP API for precomputed ATES region builds"
)]
struct Args {
    /// Directory holding region builds (one subdirectory each).
    #[arg(long, default_value = "data/regions")]
    data_dir: PathBuf,
    /// Address to listen on.
    #[arg(long, default_value = "127.0.0.1:8080")]
    bind: SocketAddr,
    /// Serve the built frontend (e.g. web/dist) at `/`.
    #[arg(long)]
    web_dir: Option<PathBuf>,
    /// Saved CAIC `products/all` JSON: adds forecast context to route
    /// reports. Read once at startup; nothing is fetched from CAIC.
    #[arg(long, requires = "caic_areas")]
    caic_products: Option<PathBuf>,
    /// Saved CAIC `products/all/area` GeoJSON (forecast zone polygons).
    #[arg(long, requires = "caic_products")]
    caic_areas: Option<PathBuf>,
    /// When the CAIC files were saved, shown with the context.
    #[arg(long)]
    caic_retrieved: Option<String>,
    /// Treeline as LOWER,UPPER metres, for elevation bands. Without it,
    /// route stretches show every band.
    #[arg(long, value_parser = parse_treeline)]
    treeline: Option<Treeline>,
}

fn parse_treeline(s: &str) -> Result<Treeline, String> {
    let (lo, hi) = s.split_once(',').ok_or("expected LOWER,UPPER in metres")?;
    let num = |v: &str| v.trim().parse::<f64>().map_err(|e| format!("{v}: {e}"));
    Treeline::new(num(lo)?, num(hi)?)
}

#[tokio::main]
async fn main() -> ExitCode {
    let args = Args::parse();
    let regions = match load_regions(&args.data_dir) {
        Ok(r) if r.is_empty() => {
            eprintln!(
                "error: no region builds in {}; run `ates build-region` first",
                args.data_dir.display()
            );
            return ExitCode::FAILURE;
        }
        Ok(r) => r,
        Err(e) => {
            eprintln!("error: {e}");
            return ExitCode::FAILURE;
        }
    };
    for r in &regions {
        eprintln!(
            "loaded region `{}`: {}x{} cells, EPSG:{}",
            r.name,
            r.grids.ates.rows(),
            r.grids.ates.cols(),
            r.epsg
        );
    }
    let forecast = match (args.caic_products, args.caic_areas) {
        (Some(products), Some(areas)) => {
            let files = CaicFiles {
                products,
                areas,
                retrieved: args.caic_retrieved,
            };
            match files.load() {
                Ok(forecasts) => {
                    eprintln!(
                        "loaded {} CAIC zone forecast(s), {} zone polygon(s); treeline {}",
                        forecasts.forecasts.len(),
                        forecasts.zones.len(),
                        args.treeline
                            .map_or("not set (all bands shown)".into(), |t| {
                                format!("{}-{} m", t.lower_m, t.upper_m)
                            })
                    );
                    Some(ForecastState {
                        forecasts,
                        treeline: args.treeline,
                    })
                }
                Err(e) => {
                    eprintln!("error: {e}");
                    return ExitCode::FAILURE;
                }
            }
        }
        _ => None,
    };
    let state = Arc::new(AppState {
        regions,
        data_dir: args.data_dir,
        web_dir: args.web_dir,
        forecast,
    });
    let listener = match tokio::net::TcpListener::bind(args.bind).await {
        Ok(l) => l,
        Err(e) => {
            eprintln!("error: cannot listen on {}: {e}", args.bind);
            return ExitCode::FAILURE;
        }
    };
    eprintln!("listening on http://{}", args.bind);
    if let Err(e) = axum::serve(listener, router(state)).await {
        eprintln!("error: {e}");
        return ExitCode::FAILURE;
    }
    ExitCode::SUCCESS
}
