use axum::{
    Router,
    extract::State,
    http::{StatusCode, header},
    response::{IntoResponse, Response},
    routing::get,
};
use prometheus_client_php_mmap::exporter::CachedDirExporter;
use std::env;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

type ExporterState = Arc<Mutex<CachedDirExporter>>;

#[tokio::main]
async fn main() {
    if let Err(err) = run().await {
        eprintln!("prometheus-mmap-exporter: {err}");
        std::process::exit(1);
    }
}

async fn run() -> Result<(), Box<dyn std::error::Error>> {
    let mut dir: Option<PathBuf> = None;
    let mut listen = String::from("127.0.0.1:9464");

    let mut args = env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--dir" => {
                dir = args.next().map(PathBuf::from);
            }
            "--listen" => {
                if let Some(value) = args.next() {
                    listen = value;
                }
            }
            "--help" | "-h" => {
                print_help();
                return Ok(());
            }
            other => {
                return Err(format!("unknown argument: {other}").into());
            }
        }
    }

    let dir = dir.ok_or("missing required --dir PATH")?;
    let exporter = Arc::new(Mutex::new(CachedDirExporter::new(&dir)?));
    let app = Router::new()
        .route("/metrics", get(metrics))
        .route("/healthz", get(healthz))
        .with_state(exporter);
    let listener = tokio::net::TcpListener::bind(&listen).await?;

    eprintln!(
        "prometheus-mmap-exporter listening on http://{listen}/metrics (dir: {})",
        dir.display()
    );

    axum::serve(listener, app).await?;
    Ok(())
}

async fn metrics(State(exporter): State<ExporterState>) -> Response {
    let mut exporter = match exporter.lock() {
        Ok(exporter) => exporter,
        Err(_) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                "exporter state is unavailable\n",
            )
                .into_response();
        }
    };

    match exporter.render() {
        Ok(body) => (
            [(
                header::CONTENT_TYPE,
                "text/plain; version=0.0.4; charset=utf-8",
            )],
            body,
        )
            .into_response(),
        Err(err) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("render error: {err}\n"),
        )
            .into_response(),
    }
}

async fn healthz() -> &'static str {
    "ok\n"
}

fn print_help() {
    println!("Usage: prometheus-mmap-exporter --dir PATH [--listen ADDR]");
    println!();
    println!("Options:");
    println!("  --dir PATH       Metrics directory to export");
    println!("  --listen ADDR    Listen address (default: 127.0.0.1:9464)");
}

#[cfg(test)]
mod tests {
    use super::{ExporterState, healthz, metrics};
    use axum::{
        body::to_bytes,
        extract::State,
        http::{StatusCode, header},
    };
    use prometheus_client_php_mmap::exporter::CachedDirExporter;
    use std::sync::{Arc, Mutex};
    use tempfile::tempdir;

    #[tokio::test]
    async fn metrics_returns_prometheus_text_content_type() {
        let dir = tempdir().unwrap();
        let exporter: ExporterState =
            Arc::new(Mutex::new(CachedDirExporter::new(dir.path()).unwrap()));

        let response = metrics(State(exporter)).await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response.headers().get(header::CONTENT_TYPE).unwrap(),
            "text/plain; version=0.0.4; charset=utf-8"
        );
        assert_eq!(
            to_bytes(response.into_body(), usize::MAX).await.unwrap(),
            "# HELP prometheus_mmap_exporter_file_errors Number of metric files skipped during this scrape\n# TYPE prometheus_mmap_exporter_file_errors gauge\nprometheus_mmap_exporter_file_errors 0\n"
        );
    }

    #[tokio::test]
    async fn healthz_returns_ok() {
        assert_eq!(healthz().await, "ok\n");
    }
}
