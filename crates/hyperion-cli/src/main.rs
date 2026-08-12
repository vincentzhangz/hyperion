use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use clap::{Parser, Subcommand};
use glommio::prelude::*;
use hyperion_browser::BrowserContext;
use hyperion_cdp::{CdpServer, CdpServerOptions};
use hyperion_config::Config;
use hyperion_net::client::HttpClientOptions;
use tracing_subscriber::EnvFilter;

#[derive(Parser)]
#[command(
    name = "hyperion",
    version,
    about = "Headless browser engine on Glommio (io_uring)"
)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Run engine smoke tests (Glommio io_uring + V8).
    Smoke,
    /// Fetch a URL, parse it, and print title + text.
    Fetch { url: String },
    /// Serve the Chrome DevTools Protocol on a host:port (default 127.0.0.1:9222).
    Serve {
        #[arg(default_value = "127.0.0.1:9222")]
        bind: String,
        /// Print the live DOM/HTML preview URL.
        #[arg(long, default_value_t = true)]
        preview: bool,
    },
}

fn main() {
    let cli = Cli::parse();
    match cli.cmd {
        Cmd::Smoke => smoke(),
        Cmd::Fetch { url } => fetch(&url),
        Cmd::Serve { bind, preview } => serve(&bind, preview),
    }
}

fn init_tracing(config: &Config) {
    let filter = EnvFilter::try_new(&config.log_level).unwrap_or_else(|_| EnvFilter::new("info"));
    let builder = tracing_subscriber::fmt().with_env_filter(filter);
    if config.log_format == "json" {
        let _ = builder.json().try_init();
    } else {
        let _ = builder.try_init();
    }
}

fn smoke() {
    println!("== hyperion smoke ==");
    glommio_smoke();
    hyperion_js::v8_smoke();
    println!("== smoke ok ==");
}

/// Spawn a Glommio LocalExecutor and exercise io_uring file I/O.
/// Fails if io_uring is unavailable or the memlock rlimit is too low.
fn glommio_smoke() {
    use glommio::io::OpenOptions;
    let path = "/tmp/hyperion-smoke.txt";

    let ex = LocalExecutorBuilder::default()
        .spawn(move || async move {
            let file = OpenOptions::new()
                .write(true)
                .create(true)
                .buffered_open(path)
                .await?;
            let n = file.write_at(b"hello io_uring".to_vec(), 0).await?;
            file.close().await?;
            assert_eq!(n, 14, "short write");
            Ok::<(), std::io::Error>(())
        })
        .expect("failed to spawn glommio executor");

    ex.join()
        .expect("glommio executor failed")
        .expect("io_uring file I/O failed");
    println!("glommio: io_uring file write OK (kernel 5.8+, memlock ok)");
}

/// Fetch `url` on a glommio executor and print the loaded page.
fn fetch(url: &str) {
    let url = url.to_owned();
    let ex = LocalExecutorBuilder::default()
        .spawn(move || async move {
            let context = BrowserContext::new(HttpClientOptions::default());
            match context.navigate(&url).await {
                Ok(page) => {
                    println!("status: {}", page.status());
                    println!("url:    {}", page.url());
                    println!("title:  {}", page.title());
                    println!("--- text ---");
                    println!("{}", page.text_content());
                }
                Err(e) => eprintln!("fetch failed: {e}"),
            }
            Ok::<(), ()>(())
        })
        .expect("failed to spawn glommio executor");
    let _ = ex.join();
}

/// Serve the CDP endpoint. With `--preview`, also prints the live DOM/HTML mirror URL.
fn serve(bind: &str, preview: bool) {
    let mut config = Config::from_env().unwrap_or_else(|e| {
        eprintln!("config error: {e}");
        std::process::exit(2);
    });
    if let Ok(addr) = bind.parse() {
        config.bind = addr;
    }
    if let Err(e) = config.validate() {
        eprintln!("config error: {e}");
        std::process::exit(2);
    }
    init_tracing(&config);
    hyperion_js::configure_v8_pool(
        config.v8_pool_size,
        config.v8_heap_mb,
        config.max_remote_objects,
        config.eval_deadline,
    );

    let shutdown = Arc::new(AtomicBool::new(false));
    let flag = shutdown.clone();
    let _ = ctrlc::set_handler(move || {
        flag.store(true, Ordering::Relaxed);
    });

    let workers = config.workers.max(1);
    let base = config.bind;
    let mut joins = Vec::new();
    for i in 0..workers {
        let mut cfg = config.clone();
        let mut addr = base;
        if workers > 1 {
            addr.set_port(base.port().saturating_add(i as u16));
        }
        cfg.bind = addr;
        let preview = preview && i == 0;
        let shutdown = shutdown.clone();
        let handle = LocalExecutorBuilder::default()
            .name(&format!("hyperion-w{i}"))
            .spawn(move || async move {
                let mut http = HttpClientOptions::default();
                http.allow_private_network = cfg.allow_private_network;
                http.max_body_bytes = cfg.max_body_bytes;
                http.fetch_pause_timeout = cfg.fetch_pause_timeout;
                http.per_origin_connections = cfg.per_origin_connections;
                let options = CdpServerOptions {
                    bind: cfg.bind,
                    http,
                    config: cfg.clone(),
                };
                let server = CdpServer::new(options).expect("failed to bind CDP server");
                let bound = server.local_addr();
                let flag = server.shutdown_flag();
                glommio::spawn_local(async move {
                    loop {
                        glommio::timer::Timer::new(std::time::Duration::from_millis(200)).await;
                        if shutdown.load(Ordering::Relaxed) {
                            flag.store(true, Ordering::Relaxed);
                            break;
                        }
                    }
                })
                .detach();
                tracing::info!(%bound, worker = i, "hyperion CDP listening");
                if preview {
                    tracing::info!(url = format!("http://{bound}/preview"), "live preview");
                    tracing::info!(url = format!("http://{bound}/ui/"), "dashboard");
                    println!("hyperion CDP listening on ws://{bound}");
                    println!("live preview:        http://{bound}/preview");
                    println!("dashboard:           http://{bound}/ui/");
                    println!("health:              http://{bound}/health/live");
                }
                server.run().await.expect("CDP server failed");
                Ok::<(), ()>(())
            })
            .expect("failed to spawn glommio executor");
        joins.push(handle);
    }
    for j in joins {
        let _ = j.join();
    }
}
