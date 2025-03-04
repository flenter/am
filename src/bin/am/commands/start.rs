use crate::dir::AutoCleanupDir;
use crate::downloader::{download_github_release, unpack, verify_checksum};
use crate::interactive;
use crate::server::start_web_server;
use anyhow::{anyhow, bail, Context, Result};
use autometrics_am::config::{endpoints_from_first_input, AmConfig};
use autometrics_am::parser::endpoint_parser;
use autometrics_am::prometheus;
use autometrics_am::prometheus::ScrapeConfig;
use clap::Parser;
use directories::ProjectDirs;
use futures_util::FutureExt;
use indicatif::MultiProgress;
use once_cell::sync::Lazy;
use rand::distributions::{Alphanumeric, DistString};
use std::fs::File;
use std::io::{Seek, SeekFrom};
use std::net::SocketAddr;
use std::path::Path;
use std::process::Stdio;
use std::time::Duration;
use std::{env, fs, vec};
use tempfile::NamedTempFile;
use tokio::sync::watch;
use tokio::sync::watch::Receiver;
use tokio::{process, select};
use tracing::{debug, error, info, warn};
use url::Url;

// Create a reqwest client that will be used to make HTTP requests. This allows
// for keep-alives if we are making multiple requests to the same host.
pub(crate) static CLIENT: Lazy<reqwest::Client> = Lazy::new(|| {
    reqwest::Client::builder()
        .user_agent(concat!("am/", env!("CARGO_PKG_VERSION")))
        .connect_timeout(Duration::from_secs(5))
        .build()
        .expect("Unable to create reqwest client")
});

#[derive(Parser, Clone)]
pub struct CliArguments {
    /// The endpoint(s) that Prometheus will scrape.
    ///
    /// Multiple endpoints can be specified by separating them with a space.
    /// The endpoint can be provided in the following formats:
    /// - `:3000`. Defaults to `http`, `localhost` and `/metrics`.
    /// - `localhost:3000`. Defaults to `http`, and `/metrics`.
    /// - `https://localhost:3000`. Defaults to `/metrics`.
    /// - `https://localhost:3000/api/metrics`. No defaults.
    #[clap(value_parser = endpoint_parser, verbatim_doc_comment)]
    metrics_endpoints: Vec<Url>,

    /// The Prometheus version to use. It will be downloaded if am has not
    /// downloaded it already.
    #[clap(
        long,
        env,
        default_value = "v2.45.0",
        help_heading = "Prometheus options"
    )]
    prometheus_version: String,

    /// The default scrape interval for all Prometheus jobs.
    ///
    /// This can be overridden on a per endpoint configuration in the am.toml file.
    #[clap(long, env, help_heading = "Prometheus options", value_parser = humantime::parse_duration)]
    scrape_interval: Option<Duration>,

    /// The listen address for the web server of am.
    ///
    /// This includes am's HTTP API, the explorer and the proxy to the Prometheus, Gateway, etc.
    #[clap(
        short,
        long,
        env,
        default_value = "127.0.0.1:6789",
        alias = "explorer-address"
    )]
    listen_address: SocketAddr,

    /// Enable pushgateway.
    ///
    /// Pushgateway accepts metrics from other applications and exposes these to
    /// Prometheus. This is useful for applications that cannot be scraped,
    /// either cause they are short-lived (like functions), or Prometheus cannot
    /// reach them (like client-side applications).
    #[clap(short, long, env, help_heading = "Pushgateway options")]
    pushgateway_enabled: Option<bool>,

    /// The pushgateway version to use.
    #[clap(
        long,
        env,
        default_value = "v1.6.0",
        help_heading = "Pushgateway options"
    )]
    pushgateway_version: String,

    /// Whenever to clean up files created by Prometheus/Pushgateway after successful execution
    #[clap(short = 'd', long, env)]
    ephemeral: bool,

    /// Whenever to *NOT* load the autometrics rules file into Prometheus
    #[clap(long, env)]
    no_rules: bool,
}

#[derive(Debug, Clone)]
struct Arguments {
    metrics_endpoints: Vec<Endpoint>,
    prometheus_version: String,
    prometheus_scrape_interval: Duration,
    listen_address: SocketAddr,
    pushgateway_enabled: bool,
    pushgateway_version: String,
    ephemeral_working_directory: bool,
    no_rules: bool,
}

impl Arguments {
    fn new(args: CliArguments, config: AmConfig) -> Self {
        Arguments {
            metrics_endpoints: endpoints_from_first_input(args.metrics_endpoints, config.endpoints)
                .into_iter()
                .filter_map(|e| e.try_into().ok())
                .collect(),
            prometheus_version: args.prometheus_version,
            listen_address: args.listen_address,
            pushgateway_enabled: args
                .pushgateway_enabled
                .or(config.pushgateway_enabled)
                .unwrap_or(false),
            pushgateway_version: args.pushgateway_version,
            ephemeral_working_directory: args.ephemeral,
            prometheus_scrape_interval: args
                .scrape_interval
                .or(config.prometheus_scrape_interval)
                .unwrap_or_else(|| Duration::from_secs(5)),
            no_rules: args.no_rules,
        }
    }
}

#[derive(Debug, Clone)]
pub struct Endpoint {
    url: Url,
    job_name: String,
    honor_labels: bool,
    scrape_interval: Option<Duration>,
}

impl Endpoint {
    fn new(
        url: Url,
        job_name: String,
        honor_labels: bool,
        scrape_interval: Option<Duration>,
    ) -> Self {
        Self {
            url,
            job_name,
            honor_labels,
            scrape_interval,
        }
    }
}

impl TryFrom<autometrics_am::config::Endpoint> for Endpoint {
    type Error = anyhow::Error;

    fn try_from(value: autometrics_am::config::Endpoint) -> Result<Self, Self::Error> {
        Ok(Self {
            url: value.url,
            job_name: value
                .job_name
                .ok_or_else(|| anyhow!("TryFrom requires job_name"))?,
            honor_labels: value.honor_labels.unwrap_or(false),
            scrape_interval: value.prometheus_scrape_interval,
        })
    }
}

impl From<Endpoint> for ScrapeConfig {
    /// Convert an InnerEndpoint to a Prometheus ScrapeConfig.
    ///
    /// Scrape config only supports http and https atm.
    fn from(endpoint: Endpoint) -> Self {
        let scheme = match endpoint.url.scheme() {
            "http" => Some(prometheus::Scheme::Http),
            "https" => Some(prometheus::Scheme::Https),
            _ => None,
        };

        let mut metrics_path = endpoint.url.path();
        if metrics_path.is_empty() {
            metrics_path = "/metrics";
        }

        let host = match endpoint.url.port() {
            Some(port) => format!("{}:{}", endpoint.url.host_str().unwrap(), port),
            None => endpoint.url.host_str().unwrap().to_string(),
        };

        ScrapeConfig {
            job_name: endpoint.job_name,
            static_configs: vec![prometheus::StaticScrapeConfig {
                targets: vec![host],
            }],
            metrics_path: Some(metrics_path.to_string()),
            scheme,
            honor_labels: Some(endpoint.honor_labels),
            scrape_interval: endpoint.scrape_interval,
        }
    }
}

pub async fn handle_command(args: CliArguments, config: AmConfig, mp: MultiProgress) -> Result<()> {
    let mut args = Arguments::new(args, config);

    if args.metrics_endpoints.is_empty() && !args.pushgateway_enabled {
        info!("No metrics endpoints provided and pushgateway is not enabled. Please provide an endpoint.");

        // Ask for a metric endpoint and parse the input like a regular CLI argument
        let url = interactive::user_input("Metric endpoint")?;
        let url = endpoint_parser(&url)?;

        // Add the provided URL with the job name am_0
        let endpoint = Endpoint::new(url, "am_0".to_string(), false, None);
        args.metrics_endpoints.push(endpoint);
    }

    // First let's retrieve the directory for our application to store data in.
    let project_dirs =
        ProjectDirs::from("", "autometrics", "am").context("Unable to determine home directory")?;
    let local_data = project_dirs.data_local_dir().to_owned();

    // Make sure that the local data directory exists for our application.
    std::fs::create_dir_all(&local_data)
        .with_context(|| format!("Unable to create data directory: {:?}", local_data))?;

    if !args.metrics_endpoints.is_empty() {
        info!("Checking if provided metrics endpoints work...");

        // check if the provided endpoint works
        for endpoint in &args.metrics_endpoints {
            if let Err(err) = check_endpoint(&endpoint.url).await {
                warn!(
                    ?err,
                    "Failed to make request to {} (job {})", endpoint.url, endpoint.job_name
                );
            }
        }
    }

    if args.pushgateway_enabled {
        let url = Url::parse("http://localhost:9091/pushgateway/metrics").unwrap();
        let endpoint = Endpoint::new(url, "am_pushgateway".to_string(), true, None);
        args.metrics_endpoints.push(endpoint);
    }

    let (tx, rx) = watch::channel(None);

    // Start web server for hosting the explorer, am api and proxies to the enabled services.
    let web_server_task = async move {
        start_web_server(
            &args.listen_address,
            true,
            args.pushgateway_enabled,
            None,
            tx,
        )
        .await
    };

    // Start Prometheus server
    let prometheus_args = args.clone();
    let prometheus_local_data = local_data.clone();
    let prometheus_multi_progress = mp.clone();

    let prom_rx = rx.clone();

    let prometheus_task = async move {
        let prometheus_version = prometheus_args.prometheus_version.trim_start_matches('v');

        info!("Using Prometheus version: {}", prometheus_version);

        let prometheus_path =
            prometheus_local_data.join(format!("prometheus-{prometheus_version}"));

        // Check if prometheus is available
        if !prometheus_path.exists() {
            info!("Cached version of Prometheus not found, downloading Prometheus");
            install_prometheus(
                &prometheus_path,
                prometheus_version,
                prometheus_multi_progress,
            )
            .await?;
            debug!("Downloaded Prometheus to: {:?}", &prometheus_path);
        } else {
            debug!("Found prometheus in: {:?}", prometheus_path);
        }

        let prometheus_config = generate_prom_config(
            prometheus_args.prometheus_scrape_interval,
            prometheus_args.metrics_endpoints,
            !args.no_rules,
        )?;

        start_prometheus(
            &prometheus_path,
            &prometheus_config,
            args.ephemeral_working_directory,
            !args.no_rules,
            prom_rx,
        )
        .await
    };

    let pushgateway_task = if args.pushgateway_enabled {
        let pushgateway_args = args.clone();
        let pushgateway_local_data = local_data.clone();
        let pushgateway_multi_progress = mp.clone();
        async move {
            let pushgateway_version = pushgateway_args.pushgateway_version.trim_start_matches('v');

            info!("Using pushgateway version: {}", pushgateway_version);

            let pushgateway_path =
                pushgateway_local_data.join(format!("pushgateway-{pushgateway_version}"));

            // Check if pushgateway is available
            if !pushgateway_path.exists() {
                info!("Cached version of pushgateway not found, downloading pushgateway");
                install_pushgateway(
                    &pushgateway_path,
                    pushgateway_version,
                    pushgateway_multi_progress,
                )
                .await?;
                debug!("Downloaded pushgateway to: {:?}", &pushgateway_path);
            } else {
                debug!("Found pushgateway in: {:?}", &pushgateway_path);
            }

            start_pushgateway(&pushgateway_path, args.ephemeral_working_directory, rx).await
        }
        .boxed()
    } else {
        async move { anyhow::Ok(()) }.boxed()
    };

    if !args.metrics_endpoints.is_empty() {
        let endpoints = args
            .metrics_endpoints
            .iter()
            .map(|endpoint| endpoint.url.to_string())
            .collect::<Vec<String>>()
            .join(", ");
        info!("Now sampling the following endpoints for metrics: {endpoints}");
    }

    select! {
        biased;

        _ = tokio::signal::ctrl_c() => {
            info!("SIGINT signal received, exiting...");
            Ok(())
        }

        Err(err) = web_server_task => {
            bail!("Web server exited with an error: {err:?}");
        }

        Err(err) = prometheus_task => {
            bail!("Prometheus exited with an error: {err:?}");
        }

        Err(err) = pushgateway_task => {
            bail!("Pushgateway exited with an error: {err:?}");
        }

        else => {
            Ok(())
        }
    }
}

/// Install the specified version of Prometheus into `prometheus_path`.
///
/// This function will first create a temporary file to download the Prometheus
/// archive into. Then it will verify the downloaded archive against the
/// downloaded checksum. Finally it will unpack the archive into
/// `prometheus_path`.
async fn install_prometheus(
    prometheus_path: &Path,
    prometheus_version: &str,
    multi_progress: MultiProgress,
) -> Result<()> {
    let (os, arch) = determine_os_and_arch()?;
    let base = format!("prometheus-{prometheus_version}.{os}-{arch}");
    let package = format!("{base}.tar.gz");
    let prefix = format!("{base}/");

    let mut prometheus_archive = NamedTempFile::new()?;

    let calculated_checksum = download_github_release(
        prometheus_archive.as_file(),
        "prometheus",
        "prometheus",
        prometheus_version,
        &package,
        &multi_progress,
    )
    .await?;

    verify_checksum(
        &calculated_checksum,
        "prometheus",
        "prometheus",
        prometheus_version,
        &package,
    )
    .await?;

    // Make sure we set the position to the beginning of the file so that we can
    // unpack it.
    prometheus_archive.as_file_mut().seek(SeekFrom::Start(0))?;

    unpack(
        prometheus_archive.as_file(),
        "prometheus",
        prometheus_path,
        &prefix,
        &multi_progress,
    )
    .await
}

/// Install the specified version of Pushgateway into `pushgateway_path`.
///
/// This function will first create a temporary file to download the Pushgateway
/// archive into. Then it will verify the downloaded archive against the
/// downloaded checksum. Finally it will unpack the archive into
/// `pushgateway_path`.
async fn install_pushgateway(
    pushgateway_path: &Path,
    pushgateway_version: &str,
    multi_progress: MultiProgress,
) -> Result<()> {
    let (os, arch) = determine_os_and_arch()?;

    let base = format!("pushgateway-{pushgateway_version}.{os}-{arch}");
    let package = format!("{base}.tar.gz");
    let prefix = format!("{base}/");

    let mut pushgateway_archive = NamedTempFile::new()?;

    let calculated_checksum = download_github_release(
        pushgateway_archive.as_file(),
        "prometheus",
        "pushgateway",
        pushgateway_version,
        &package,
        &multi_progress,
    )
    .await?;

    verify_checksum(
        &calculated_checksum,
        "prometheus",
        "pushgateway",
        pushgateway_version,
        &package,
    )
    .await?;

    // Make sure we set the position to the beginning of the file so that we can
    // unpack it.
    pushgateway_archive.as_file_mut().seek(SeekFrom::Start(0))?;

    unpack(
        pushgateway_archive.as_file(),
        "pushgateway",
        pushgateway_path,
        &prefix,
        &multi_progress,
    )
    .await
}

/// Translates the OS and arch provided by Rust to the convention used by
/// Prometheus.
fn determine_os_and_arch() -> Result<(&'static str, &'static str)> {
    use std::env::consts::{ARCH, OS};

    let os = match OS {
        "linux" => "linux",
        "macos" => "darwin",
        "windows" => "windows",
        "freebsd" => "freebsd",
        "netbsd" => "netbsd",
        "openbsd" => "openbsd",
        "dragonfly" => "dragonfly",
        _ => bail!(format!("Unsupported OS: {}", ARCH)),
    };

    let arch = match ARCH {
        "x86" => "386",
        "x86_64" => "amd64",
        "aarch64" => "arm64",
        "s390x" => "s390x",
        "powerpc64" => "powerpc64", // NOTE: Do we use this one, or the le one?
        // "mips" => "mips", // NOTE: Not sure which mips to pick in this situation
        // "arm" => "arm", // NOTE: Not sure which arm to pick in this situation
        _ => bail!(format!("Unsupported architecture: {}", ARCH)),
    };

    Ok((os, arch))
}

/// Generate a Prometheus configuration file.
///
/// For now this will expand a simple template and only has support for a single
/// endpoint.
fn generate_prom_config(
    scrape_interval: Duration,
    metric_endpoints: Vec<Endpoint>,
    enable_rules: bool,
) -> Result<prometheus::Config> {
    let scrape_configs = metric_endpoints.into_iter().map(Into::into).collect();

    let mut rule_files = Vec::new();

    if enable_rules {
        let path = env::temp_dir().join("autometrics.rules.yml");
        let path_str = path
            .into_os_string()
            .into_string()
            .map_err(|_| anyhow!("failed to convert OsString into String"))?;

        rule_files.push(path_str);
    }

    Ok(prometheus::Config {
        global: prometheus::GlobalConfig {
            scrape_interval,
            evaluation_interval: "15s".to_string(),
        },
        scrape_configs,
        rule_files,
    })
}

/// Checks whenever the endpoint works
async fn check_endpoint(url: &Url) -> Result<()> {
    let response = CLIENT
        .get(url.as_str())
        .timeout(Duration::from_secs(5))
        .send()
        .await?;

    if !response.status().is_success() {
        bail!("endpoint did not return 2xx status code");
    }

    Ok(())
}

/// Start a prometheus process. This will block until the Prometheus process
/// stops.
async fn start_prometheus(
    prometheus_path: &Path,
    prometheus_config: &prometheus::Config,
    ephemeral: bool,
    enable_rules: bool,
    mut rx: Receiver<Option<SocketAddr>>,
) -> Result<()> {
    // First write needed files to temp
    let runtime_dir = AutoCleanupDir::new(
        &format!(
            "am-prometheus-{}",
            Alphanumeric.sample_string(&mut rand::thread_rng(), 6)
        ),
        true,
    )?;

    let config_file_path = runtime_dir.join("prometheus.yml");
    let config_file = File::create(&config_file_path)?;

    debug!(
        path = ?config_file_path,
        "Created temporary file for Prometheus config serialization"
    );

    serde_yaml::to_writer(&config_file, &prometheus_config)?;

    if enable_rules {
        let rule_file = env::temp_dir().join("autometrics.rules.yml");
        fs::write(
            rule_file,
            include_bytes!("../../../../files/autometrics-shared/autometrics.rules.yml"),
        )?;
    }

    // TODO: Capture prometheus output into a internal buffer and expose it
    // through an api.

    let work_dir = AutoCleanupDir::new("prometheus", ephemeral)?;

    #[cfg(not(target_os = "windows"))]
    let program = "prometheus";
    #[cfg(target_os = "windows")]
    let program = "prometheus.exe";

    let prometheus_path = prometheus_path.join(program);

    info!(bin_path = ?prometheus_path.display(), "Starting prometheus");

    let external_url = rx.wait_for(Option::is_some).await.map_or_else(
        |_| "localhost:6789".to_string(),
        |address| address.unwrap().to_string(),
    );

    let child = process::Command::new(prometheus_path)
        .arg(format!("--config.file={}", config_file_path.display()))
        .arg("--web.listen-address=:9090")
        .arg("--web.enable-lifecycle")
        .arg(format!(
            "--web.external-url=http://{external_url}/prometheus"
        ))
        .arg("--web.enable-remote-write-receiver")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .current_dir(&work_dir)
        .spawn()
        .context("Unable to start Prometheus")?
        .wait_with_output()
        .await?;

    if !child.status.success() {
        if !child.stdout.is_empty() {
            error!("Prometheus stdout:\n{}", String::from_utf8(child.stdout)?);
        }

        if !child.stderr.is_empty() {
            error!("Prometheus stderr:\n{}", String::from_utf8(child.stderr)?);
        }

        bail!("Prometheus exited with status {}", child.status)
    }

    Ok(())
}

/// Start a prometheus process. This will block until the Prometheus process
/// stops.
async fn start_pushgateway(
    pushgateway_path: &Path,
    ephemeral: bool,
    mut rx: Receiver<Option<SocketAddr>>,
) -> Result<()> {
    let work_dir = AutoCleanupDir::new("pushgateway", ephemeral)?;

    let external_url = rx.wait_for(Option::is_some).await.map_or_else(
        |_| "localhost:6789".to_string(),
        |address| address.unwrap().to_string(),
    );

    info!("Starting Pushgateway");
    let child = process::Command::new(pushgateway_path.join("pushgateway"))
        .arg("--web.listen-address=:9091")
        .arg(format!(
            "--web.external-url=http://{external_url}/pushgateway"
        ))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .current_dir(&work_dir)
        .spawn()
        .context("Unable to start Pushgateway")?
        .wait_with_output()
        .await?;

    if !child.status.success() {
        if !child.stdout.is_empty() {
            error!("Pushgateway stdout:\n{}", String::from_utf8(child.stdout)?);
        }

        if !child.stderr.is_empty() {
            error!("Pushgateway stderr:\n{}", String::from_utf8(child.stderr)?);
        }

        bail!("Pushgateway exited with status {}", child.status)
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use rstest::rstest;

    #[rstest]
    #[case("127.0.0.1", "http://127.0.0.1:80/metrics")]
    #[case("https://127.0.0.1", "https://127.0.0.1:443/metrics")]
    #[case("localhost:3030", "http://localhost:3030/metrics")]
    #[case("localhost:3030/api/metrics", "http://localhost:3030/api/metrics")]
    #[case(
        "localhost:3030/api/observability",
        "http://localhost:3030/api/observability"
    )]
    #[case(":3000", "http://localhost:3000/metrics")]
    #[case(":3030/api/observability", "http://localhost:3030/api/observability")]
    fn endpoint_parser_ok(#[case] input: &str, #[case] expected: url::Url) {
        let result = super::endpoint_parser(input).expect("expected no error");
        assert_eq!(expected, result);
    }

    #[rstest]
    #[case("ftp://localhost")]
    #[case("not a valid url at all")]
    fn endpoint_parser_error(#[case] input: &str) {
        let _ = super::endpoint_parser(input).expect_err("expected a error");
        // We're not checking which specific error occurred, just that a error
        // occurred.
    }
}
