//! Web3_proxy is a fast caching and load balancing proxy for web3 (Ethereum or similar) JsonRPC servers.
//!
//! Signed transactions (eth_sendRawTransaction) are sent in parallel to the configured private RPCs (eden, ethermine, flashbots, etc.).
//!
//! All other requests are sent to an RPC server on the latest block (alchemy, moralis, rivet, your own node, or one of many other providers).
//! If multiple servers are in sync, the fastest server is prioritized. Since the fastest server is most likely to serve requests, slow servers are unlikely to ever get any requests.

//#![warn(missing_docs)]
#![forbid(unsafe_code)]

use anyhow::Context;
use futures::StreamExt;
use log::{debug, error, info, warn};
use num::Zero;
use parking_lot::deadlock;
use std::fs;
use std::path::Path;
use std::sync::atomic::{self, AtomicUsize};
use std::thread;
use tokio::runtime;
use tokio::sync::broadcast;
use tokio::time::Duration;
use web3_proxy::app::{flatten_handle, flatten_handles, Web3ProxyApp};
use web3_proxy::config::{CliConfig, TopConfig};
use web3_proxy::{frontend, metrics_frontend};

fn run(
    shutdown_sender: broadcast::Sender<()>,
    cli_config: CliConfig,
    top_config: TopConfig,
) -> anyhow::Result<()> {
    debug!("{:?}", cli_config);
    debug!("{:?}", top_config);

    let mut shutdown_receiver = shutdown_sender.subscribe();

    // spawn a thread for deadlock detection
    // TODO: disable this feature during release mode and things should go faster
    thread::spawn(move || loop {
        thread::sleep(Duration::from_secs(10));
        let deadlocks = deadlock::check_deadlock();
        if deadlocks.is_empty() {
            continue;
        }

        println!("{} deadlocks detected", deadlocks.len());
        for (i, threads) in deadlocks.iter().enumerate() {
            println!("Deadlock #{}", i);
            for t in threads {
                println!("Thread Id {:#?}", t.thread_id());
                println!("{:#?}", t.backtrace());
            }
        }
    });

    // set up tokio's async runtime
    let mut rt_builder = runtime::Builder::new_multi_thread();

    let chain_id = top_config.app.chain_id;
    rt_builder.enable_all().thread_name_fn(move || {
        static ATOMIC_ID: AtomicUsize = AtomicUsize::new(0);
        // TODO: what ordering? i think we want seqcst so that these all happen in order, but that might be stricter than we really need
        let worker_id = ATOMIC_ID.fetch_add(1, atomic::Ordering::SeqCst);
        // TODO: i think these max at 15 characters
        format!("web3-{}-{}", chain_id, worker_id)
    });

    if cli_config.workers > 0 {
        rt_builder.worker_threads(cli_config.workers);
    }

    // start tokio's async runtime
    let rt = rt_builder.build()?;

    let num_workers = rt.metrics().num_workers();
    info!("num_workers: {}", num_workers);

    rt.block_on(async {
        let app_frontend_port = cli_config.port;
        let app_prometheus_port = cli_config.prometheus_port;

        // start the main app
        let mut spawned_app =
            Web3ProxyApp::spawn(top_config, num_workers, shutdown_sender.subscribe()).await?;

        let frontend_handle =
            tokio::spawn(frontend::serve(app_frontend_port, spawned_app.app.clone()));

        let prometheus_handle = tokio::spawn(metrics_frontend::serve(
            spawned_app.app,
            app_prometheus_port,
        ));

        // if everything is working, these should both run forever
        tokio::select! {
            x = flatten_handles(spawned_app.app_handles) => {
                match x {
                    Ok(_) => info!("app_handle exited"),
                    Err(e) => {
                        return Err(e);
                    }
                }
            }
            x = flatten_handle(frontend_handle) => {
                match x {
                    Ok(_) => info!("frontend exited"),
                    Err(e) => {
                        return Err(e);
                    }
                }
            }
            x = flatten_handle(prometheus_handle) => {
                match x {
                    Ok(_) => info!("prometheus exited"),
                    Err(e) => {
                        return Err(e);
                    }
                }
            }
            x = tokio::signal::ctrl_c() => {
                match x {
                    Ok(_) => info!("quiting from ctrl-c"),
                    Err(e) => {
                        return Err(e.into());
                    }
                }
            }
            x = shutdown_receiver.recv() => {
                match x {
                    Ok(_) => info!("quiting from shutdown receiver"),
                    Err(e) => {
                        return Err(e.into());
                    }
                }
            }
        };

        // one of the handles stopped. send a value so the others know to shut down
        if let Err(err) = shutdown_sender.send(()) {
            warn!("shutdown sender err={:?}", err);
        };

        // wait for things like saving stats to the database to complete
        info!("waiting on important background tasks");
        let mut background_errors = 0;
        while let Some(x) = spawned_app.background_handles.next().await {
            match x {
                Err(e) => {
                    error!("{:?}", e);
                    background_errors += 1;
                }
                Ok(Err(e)) => {
                    error!("{:?}", e);
                    background_errors += 1;
                }
                Ok(Ok(_)) => continue,
            }
        }

        if background_errors.is_zero() {
            info!("finished");
        } else {
            // TODO: collect instead?
            error!("finished with errors!")
        }

        Ok(())
    })
}

fn main() -> anyhow::Result<()> {
    // if RUST_LOG isn't set, configure a default
    let rust_log = match std::env::var("RUST_LOG") {
        Ok(x) => x,
        Err(_) => "info,ethers=debug,redis_rate_limit=debug,web3_proxy=debug".to_string(),
    };

    // this probably won't matter for us in docker, but better safe than sorry
    fdlimit::raise_fd_limit();

    // initial configuration from flags
    let cli_config: CliConfig = argh::from_env();

    // convert to absolute path so error logging is most helpful
    let config_path = Path::new(&cli_config.config)
        .canonicalize()
        .context(format!(
            "checking full path of {} and {}",
            ".", // TODO: get cwd somehow
            cli_config.config
        ))?;

    // advanced configuration is on disk
    let top_config: String = fs::read_to_string(config_path.clone())
        .context(format!("reading config at {}", config_path.display()))?;
    let top_config: TopConfig = toml::from_str(&top_config)
        .context(format!("parsing config at {}", config_path.display()))?;

    // TODO: this doesn't seem to do anything
    proctitle::set_title(format!("web3_proxy-{}", top_config.app.chain_id));

    let logger = env_logger::builder().parse_filters(&rust_log).build();

    let max_level = logger.filter();

    // connect to sentry for error reporting
    // if no sentry, only log to stdout
    let _sentry_guard = if let Some(sentry_url) = top_config.app.sentry_url.clone() {
        let logger = sentry::integrations::log::SentryLogger::with_dest(logger);

        log::set_boxed_logger(Box::new(logger)).unwrap();

        let guard = sentry::init((
            sentry_url,
            sentry::ClientOptions {
                release: sentry::release_name!(),
                // TODO: Set this a to lower value (from config) in production
                traces_sample_rate: 1.0,
                ..Default::default()
            },
        ));

        Some(guard)
    } else {
        log::set_boxed_logger(Box::new(logger)).unwrap();

        None
    };

    log::set_max_level(max_level);

    // we used to do this earlier, but now we attach sentry
    debug!("CLI config @ {:#?}", cli_config.config);

    // tokio has code for catching ctrl+c so we use that
    // this shutdown sender is currently only used in tests, but we might make a /shutdown endpoint or something
    // we do not need this receiver. new receivers are made by `shutdown_sender.subscribe()`
    let (shutdown_sender, _) = broadcast::channel(1);

    run(shutdown_sender, cli_config, top_config)
}

#[cfg(test)]
mod tests {
    use ethers::{
        prelude::{Http, Provider, U256},
        utils::Anvil,
    };
    use hashbrown::HashMap;
    use std::env;

    use web3_proxy::{
        config::{AppConfig, Web3ConnectionConfig},
        rpcs::blockchain::ArcBlock,
    };

    use super::*;

    #[tokio::test]
    async fn it_works() {
        // TODO: move basic setup into a test fixture
        let path = env::var("PATH").unwrap();

        println!("path: {}", path);

        // TODO: how should we handle logs in this?
        // TODO: option for super verbose logs
        std::env::set_var("RUST_LOG", "info,web3_proxy=debug");

        let _ = env_logger::builder().is_test(true).try_init();

        let anvil = Anvil::new().spawn();

        println!("Anvil running at `{}`", anvil.endpoint());

        let anvil_provider = Provider::<Http>::try_from(anvil.endpoint()).unwrap();

        // mine a block because my code doesn't like being on block 0
        // TODO: make block 0 okay? is it okay now?
        let _: U256 = anvil_provider
            .request("evm_mine", None::<()>)
            .await
            .unwrap();

        // make a test CliConfig
        let cli_config = CliConfig {
            port: 0,
            prometheus_port: 0,
            workers: 4,
            config: "./does/not/exist/test.toml".to_string(),
            cookie_key_filename: "./does/not/exist/development_cookie_key".to_string(),
        };

        // make a test TopConfig
        // TODO: load TopConfig from a file? CliConfig could have `cli_config.load_top_config`. would need to inject our endpoint ports
        let top_config = TopConfig {
            app: AppConfig {
                chain_id: 31337,
                default_user_max_requests_per_period: Some(6_000_000),
                min_sum_soft_limit: 1,
                min_synced_rpcs: 1,
                public_requests_per_period: Some(1_000_000),
                response_cache_max_bytes: 10_usize.pow(7),
                redirect_public_url: Some("example.com/".to_string()),
                redirect_rpc_key_url: Some("example.com/{{rpc_key_id}}".to_string()),
                ..Default::default()
            },
            balanced_rpcs: HashMap::from([
                (
                    "anvil".to_string(),
                    Web3ConnectionConfig {
                        disabled: false,
                        display_name: None,
                        url: anvil.endpoint(),
                        block_data_limit: None,
                        soft_limit: 100,
                        hard_limit: None,
                        tier: 0,
                        subscribe_txs: Some(false),
                        extra: Default::default(),
                    },
                ),
                (
                    "anvil_ws".to_string(),
                    Web3ConnectionConfig {
                        disabled: false,
                        display_name: None,
                        url: anvil.ws_endpoint(),
                        block_data_limit: None,
                        soft_limit: 100,
                        hard_limit: None,
                        tier: 0,
                        subscribe_txs: Some(false),
                        extra: Default::default(),
                    },
                ),
            ]),
            private_rpcs: None,
            extra: Default::default(),
        };

        let (shutdown_sender, _) = broadcast::channel(1);

        // spawn another thread for running the app
        // TODO: allow launching into the local tokio runtime instead of creating a new one?
        let handle = {
            let shutdown_sender = shutdown_sender.clone();

            thread::spawn(move || run(shutdown_sender, cli_config, top_config))
        };

        // TODO: do something to the node. query latest block, mine another block, query again
        let proxy_provider = Provider::<Http>::try_from(anvil.endpoint()).unwrap();

        let anvil_result = anvil_provider
            .request::<_, Option<ArcBlock>>("eth_getBlockByNumber", ("latest", true))
            .await
            .unwrap()
            .unwrap();
        let proxy_result = proxy_provider
            .request::<_, Option<ArcBlock>>("eth_getBlockByNumber", ("latest", true))
            .await
            .unwrap()
            .unwrap();

        assert_eq!(anvil_result, proxy_result);

        let first_block_num = anvil_result.number.unwrap();

        let _: U256 = anvil_provider
            .request("evm_mine", None::<()>)
            .await
            .unwrap();

        let anvil_result = anvil_provider
            .request::<_, Option<ArcBlock>>("eth_getBlockByNumber", ("latest", true))
            .await
            .unwrap()
            .unwrap();
        let proxy_result = proxy_provider
            .request::<_, Option<ArcBlock>>("eth_getBlockByNumber", ("latest", true))
            .await
            .unwrap()
            .unwrap();

        assert_eq!(anvil_result, proxy_result);

        let second_block_num = anvil_result.number.unwrap();

        assert_eq!(first_block_num, second_block_num - 1);

        // tell the test app to shut down
        shutdown_sender.send(()).unwrap();

        println!("waiting for shutdown...");
        // TODO: panic if a timeout is reached
        handle.join().unwrap().unwrap();
    }
}
