use anyhow::{Context, Result};
use autometrics_am::prometheus;
use axum::body::{self, Body};
use axum::extract::Path;
use axum::response::{IntoResponse, Response};
use axum::routing::{any, get};
use axum::Router;
use clap::Parser;
use directories::ProjectDirs;
use flate2::read::GzDecoder;
use futures_util::future::join_all;
use http::{StatusCode, Uri};
use hyper::client::HttpConnector;
use include_dir::{include_dir, Dir};
use once_cell::sync::Lazy;
use serde::Deserialize;
use std::fs::File;
use std::io::{self, BufWriter, Write};
use std::net::SocketAddr;
use std::os::unix::prelude::PermissionsExt;
use std::path::PathBuf;
use std::vec;
use tokio::process;
use tracing::{debug, error, info, trace};
use url::Url;

// Create a reqwest client that will be used to make HTTP requests. This allows
// for keep-alives if we are making multiple requests to the same host.
static CLIENT: Lazy<reqwest::Client> = Lazy::new(|| {
    reqwest::Client::builder()
        .user_agent("am/0.1.0")
        .build()
        .expect("Unable to create reqwest client")
});

// TODO: add support for tls; the hyper client does not come with TLS support.
//       maybe we should just use reqwest for everything?
// Create a hyper client that will be used to make HTTP requests. This allows
// for keep-alives if we are making multiple requests to the same host.
static HYPER_CLIENT: Lazy<hyper::client::Client<HttpConnector>> =
    Lazy::new(hyper::client::Client::new);

#[derive(Parser, Clone)]
pub struct Arguments {
    /// The endpoints to scrape metrics from.
    metrics_endpoints: Vec<Url>,

    /// The prometheus version to use. Leave empty to use the latest version.
    #[clap(long, env)]
    prometheus_version: Option<String>,

    #[clap(short, long, env, default_value = "127.0.0.1:6789")]
    listen_address: SocketAddr,
}

pub async fn handle_command(args: Arguments) -> Result<()> {
    // First let's create a directory for our application to store data in.
    let project_dirs =
        ProjectDirs::from("", "autometrics", "am").context("Unable to determine home directory")?;
    let local_data = project_dirs.data_local_dir().to_owned();

    // Make sure that the local data directory exists for our application.
    std::fs::create_dir_all(&local_data)
        .context(format!("Unable to create data directory: {:?}", local_data))?;

    let mut handles = vec![];

    // Start Prometheus server
    let prometheus_args = args.clone();
    let prometheus_local_data = local_data.clone();
    let prometheus_handle = tokio::spawn(async move {
        let prometheus_version = match prometheus_args.prometheus_version {
            Some(version) => version,
            None => get_latest_prometheus_version().await?,
        };

        info!("Using Prometheus version: {}", prometheus_version);

        let prometheus_binary_path =
            prometheus_local_data.join(format!("prometheus_{}", prometheus_version));

        // Check if prom is available at "some" location
        if !prometheus_binary_path.exists() {
            info!("Downloading prometheus");
            download_prometheus(&prometheus_binary_path, &prometheus_version).await?;
            info!("Downloaded to: {:?}", &prometheus_binary_path);
        }

        let prometheus_config = generate_prom_config(prometheus_args.metrics_endpoints)?;
        start_prometheus(&prometheus_binary_path, &prometheus_config).await
    });
    handles.push(prometheus_handle);

    // Start web server for hosting the explorer, am api and proxies to the enabled services.
    let listen_address = args.listen_address;
    let web_server_handle = tokio::spawn(async move { start_web_server(&listen_address).await });
    handles.push(web_server_handle);

    join_all(handles).await;

    Ok(())
}

#[derive(Deserialize)]
struct LatestRelease {
    tag_name: String,
}

/// Retrieve the latest version of Prometheus.
///
/// This fn will resolve the release that is tagged as "latest" from GitHub and
/// then return the version that is associated with that release. It will strip
/// any leading "v" from the version string.
async fn get_latest_prometheus_version() -> Result<String> {
    debug!("Determining latest version of Prometheus");

    let version = CLIENT
        .get("https://api.github.com/repos/prometheus/prometheus/releases/latest")
        .send()
        .await
        .context("Unable to retrieve latest version of Prometheus")?
        .json::<LatestRelease>()
        .await
        .context("Unable to parse GitHub response")?
        .tag_name
        .trim_start_matches('v')
        .to_string();

    Ok(version)
}

/// Download the specified Prometheus version from GitHub and extract the
/// prometheus binary to `prometheus_binary_path`.
async fn download_prometheus(
    prometheus_binary_path: &PathBuf,
    prometheus_version: &str,
) -> Result<()> {
    let (os, arch) = determine_os_and_arch();
    // TODO: Grab the checksum file and retrieve the checksum for the archive
    let archive_path = {
        let tmp_file = tempfile::NamedTempFile::new()?;
        let mut res = CLIENT
            .get(format!("https://github.com/prometheus/prometheus/releases/download/v{prometheus_version}/prometheus-{prometheus_version}.{os}-{arch}.tar.gz"))
            .send()
            .await?
            .error_for_status()?;

        let file = File::create(&tmp_file)?;
        let mut buffer = BufWriter::new(file);

        while let Some(ref chunk) = res.chunk().await? {
            buffer.write_all(chunk)?;
        }

        tmp_file
    };

    let file = File::open(archive_path)?;
    let tar_file = GzDecoder::new(file);
    let mut ar = tar::Archive::new(tar_file);
    for entry in ar.entries()? {
        let mut entry = entry?;
        if entry.path()?.ends_with("prometheus") {
            let mut dst_file = File::create(prometheus_binary_path)?;
            io::copy(&mut entry, &mut dst_file).context("Copying to file")?;

            let mut perms = dst_file.metadata()?.permissions();
            perms.set_mode(0o755); // TODO: this will only work on unix
            dst_file.set_permissions(perms)?;

            break;
        }
    }

    Ok(())
}

/// Translates the OS and arch provided by Rust to the convention used by
/// Prometheus.
fn determine_os_and_arch() -> (&'static str, &'static str) {
    use std::env::consts::{ARCH, OS};

    let os = match OS {
        "linux" => "linux",
        "macos" => "darwin",
        "dragonfly" => "dragonfly",
        "freebsd" => "freebsd",
        "netbsd" => "netbsd",
        "openbsd" => "openbsd",
        "windows" => "windows",
        _ => panic!("Unsupported OS: {}", OS),
    };

    let arch = match ARCH {
        "x86" => "386",
        "x86_64" => "amd64",
        "aarch64" => "arm64",
        "s390x" => "s390x",
        "powerpc64" => "powerpc64", // NOTE: Do we use this one, or the le one?
        // "mips" => "mips", // NOTE: Not sure which mips to pick in this situation
        // "arm" => "arm", // NOTE: Not sure which arm to pick in this situation
        _ => panic!("Unsupported architecture: {}", ARCH),
    };

    (os, arch)
}

/// Generate a Prometheus configuration file.
///
/// For now this will expand a simple template and only has support for a single
/// endpoint.
fn generate_prom_config(metric_endpoints: Vec<Url>) -> Result<prometheus::Config> {
    let scrape_configs = metric_endpoints.iter().map(to_scrape_config).collect();

    let config = prometheus::Config {
        global: prometheus::GlobalConfig {
            scrape_interval: "15s".to_string(),
            evaluation_interval: "15s".to_string(),
        },
        scrape_configs,
    };

    Ok(config)
}

/// Convert an URL to a metric endpoint.
///
/// Scrape config only supports http and https atm.
fn to_scrape_config(metric_endpoint: &Url) -> prometheus::ScrapeConfig {
    let scheme = match metric_endpoint.scheme() {
        "http" => Some(prometheus::Scheme::Http),
        "https" => Some(prometheus::Scheme::Https),
        _ => None,
    };

    let mut metrics_path = metric_endpoint.path();
    if metrics_path.is_empty() {
        metrics_path = "/metrics";
    }

    let host = match metric_endpoint.port() {
        Some(port) => format!("{}:{}", metric_endpoint.host_str().unwrap(), port),
        None => metric_endpoint.host_str().unwrap().to_string(),
    };

    prometheus::ScrapeConfig {
        job_name: "app".to_string(),
        static_configs: vec![prometheus::StaticScrapeConfig {
            targets: vec![host],
        }],
        metrics_path: Some(metrics_path.to_string()),
        scheme,
    }
}

/// Start a prometheus process. This will block until the Prometheus process
/// stops.
async fn start_prometheus(
    prometheus_binary_path: &PathBuf,
    prometheus_config: &prometheus::Config,
) -> Result<()> {
    // First write the config to a temp file

    let config_file_path = PathBuf::from("/tmp/prometheus.yml");
    let config_file = File::create(&config_file_path)?;
    debug!(
        path = ?config_file_path,
        "Created temporary file for Prometheus config serialization"
    );
    serde_yaml::to_writer(&config_file, &prometheus_config)?;

    // TODO: Capture prometheus output into a internal buffer and expose it
    // through an api.
    // TODO: Change the working directory, maybe make it configurable?

    info!("Starting prometheus");
    let mut child = process::Command::new(prometheus_binary_path)
        .arg(format!("--config.file={}", config_file_path.display()))
        .arg("--web.listen-address=:9090")
        .arg("--web.enable-lifecycle")
        .arg("--web.external-url=http://localhost:6789/prometheus") // TODO: Make sure this matches with that is actually running.
        .spawn()
        .context("Unable to start Prometheus")?;

    let status = child.wait().await?;
    if !status.success() {
        anyhow::bail!("Prometheus exited with status {}", status)
    }

    Ok(())
}

async fn start_web_server(listen_address: &SocketAddr) -> Result<()> {
    let app = Router::new()
        // .route("/api/ ... ") // This can expose endpoints that the ui app can call
        .route("/explorer/*path", get(explorer_handler))
        .route("/prometheus/*path", any(prometheus_handler));

    let server = axum::Server::try_bind(listen_address)
        .with_context(|| format!("failed to bind to {}", listen_address))?
        .serve(app.into_make_service());

    debug!("Web server listening on {}", server.local_addr());

    // TODO: Add support for graceful shutdown
    // server.with_graceful_shutdown(shutdown_signal()).await?;
    server.await?;

    Ok(())
}

static STATIC_DIR: Dir<'_> = include_dir!("$CARGO_MANIFEST_DIR/files/explorer");

async fn explorer_handler(Path(path): Path<String>) -> impl IntoResponse {
    let path = path.trim_start_matches('/');

    trace!("Serving static file {}", path);

    match STATIC_DIR.get_file(path) {
        None => StatusCode::NOT_FOUND.into_response(),
        Some(file) => Response::builder()
            .status(StatusCode::OK)
            .body(body::boxed(body::Full::from(file.contents())))
            .unwrap(),
    }
}

async fn prometheus_handler(mut req: http::Request<Body>) -> impl IntoResponse {
    let path_query = req
        .uri()
        .path_and_query()
        .map(|v| v.as_str())
        .unwrap_or_else(|| req.uri().path());

    // TODO hardcoded for now
    let uri = format!("http://127.0.0.1:9090{}", path_query);

    trace!("Proxying request to {}", uri);

    *req.uri_mut() = Uri::try_from(uri).unwrap();

    let res = HYPER_CLIENT.request(req).await;

    // TODO: Maybe we can add a warn! if the response was not 2xx

    match res {
        Ok(res) => res.into_response(),
        Err(err) => {
            error!("Error proxying request: {:?}", err);
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}
