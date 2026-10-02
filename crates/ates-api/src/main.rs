//! `ates-api`: serve region builds over HTTP.
//!
//! ```text
//! ates-api --data-dir data/regions --bind 127.0.0.1:8080
//! ```

use std::net::SocketAddr;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;

use ates_api::regions::load_regions;
use ates_api::{AppState, router};
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
    let state = Arc::new(AppState {
        regions,
        data_dir: args.data_dir,
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
