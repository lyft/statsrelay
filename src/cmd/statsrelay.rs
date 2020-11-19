use anyhow::Context;
use stream_cancel::Tripwire;
use structopt::StructOpt;
use tokio::runtime;
use tokio::select;
use tokio::signal::unix::{signal, SignalKind};

use env_logger::Env;
use log::{debug, info};
use metrics_runtime::Receiver;

use statsrelay::backends;
use statsrelay::statsd_server;

#[derive(StructOpt, Debug)]
struct Options {
    #[structopt(short = "c", long = "--config", default_value = "/etc/statsrelay.json")]
    pub config: String,

    #[structopt(short = "t", long = "--threaded")]
    pub threaded: bool,
}

fn main() -> anyhow::Result<()> {
    env_logger::Builder::from_env(Env::default().default_filter_or("info")).init();
    let opts = Options::from_args();

    info!(
        "statsrelay loading - {} - {}",
        statsrelay::built_info::PKG_VERSION,
        statsrelay::built_info::GIT_COMMIT_HASH.unwrap_or("unknown")
    );

    let config = statsrelay::config::load_legacy_config(opts.config.as_ref())
        .with_context(|| format!("can't load config file from {}", opts.config))?;
    info!("loaded config file {}", opts.config);
    debug!("bind address: {}", config.statsd.bind);

    let receiver = Receiver::builder().build()?;
    receiver.install();

    debug!("installed metrics receiver");

    let mut builder = &mut runtime::Builder::new();
    builder = match opts.threaded {
        true => builder.threaded_scheduler(),
        false => builder.basic_scheduler(),
    };

    let mut runtime = builder.enable_all().build().unwrap();
    debug!("built tokio runtime");

    runtime.block_on(async move {
        let backends = backends::Backends::new();
        if config.statsd.shard_map.len() > 0 {
            backends.add_statsd_backend(&statsrelay::config::StatsdDuplicateTo::from_shards(
                config.statsd.shard_map.clone(),
            ));
        }
        let (sender, tripwire) = Tripwire::new();
        let run = statsd_server::run(tripwire, config.statsd.bind.clone(), backends);

        // Trap ctrl+c and sigterm messages and perform a clean shutdown
        let mut sigint = signal(SignalKind::interrupt()).unwrap();
        let mut sigterm = signal(SignalKind::terminate()).unwrap();
        tokio::spawn(async move {
            select! {
            _ = sigint.recv() => info!("received sigint"),
            _ = sigterm.recv() => info!("received sigterm"),
            }
            sender.cancel();
        });

        run.await;
    });

    drop(runtime);
    info!("runtime terminated");
    Ok(())
}
