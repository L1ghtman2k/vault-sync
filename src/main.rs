use std::{thread, time};
use std::error::Error;
use std::net::TcpListener;
use std::sync::{Arc, Mutex};
use std::sync::mpsc;

use clap::{crate_authors, crate_version, Arg, App};
use ctrlc;
use hashicorp_vault::client::error::Result as VaultResult;
use log::{error, info};
use simplelog::*;

use config::{VaultHost, VaultSyncConfig};
use vault::VaultClient;

mod audit;
mod config;
mod sync;
mod vault;

fn main() -> Result<(), Box<dyn Error>> {
    TermLogger::init(LevelFilter::Info, Config::default(), TerminalMode::Mixed)?;

    let matches = App::new("vault-sync")
        .author(crate_authors!())
        .version(crate_version!())
        .arg(Arg::with_name("config")
            .long("config")
            .value_name("FILE")
            .help("Configuration file")
            .default_value("./vault-sync.yaml")
            .takes_value(true))
        .arg(Arg::with_name("dry-run")
            .long("dry-run")
            .help("Do not do any changes with the destination Vault"))
        .get_matches();

    let config = load_config(matches.value_of("config").unwrap())?;
    let device_name = config.id.clone();

    info!("Connecting to {}", &config.src.host.url);
    let src_client = vault_client(&config.src.host)?;
    let shared_src_client = Arc::new(Mutex::new(src_client));
    let src_token = token_worker(&config.src.host, shared_src_client.clone());

    info!("Connecting to {}", &config.dst.host.url);
    let dst_client = vault_client(&config.dst.host)?;
    let shared_dst_client = Arc::new(Mutex::new(dst_client));
    let dst_token = token_worker(&config.dst.host, shared_dst_client.clone());

    let (tx, rx): (mpsc::Sender<sync::SecretOp>, mpsc::Receiver<sync::SecretOp>) = mpsc::channel();
    let sync = sync_worker(
        rx,
        &config.src.prefix,
        &config.dst.prefix,
        shared_src_client.clone(),
        shared_dst_client.clone(),
        matches.is_present("dry-run")
    );

    delete_audit_device(&device_name, &shared_src_client.clone().lock().unwrap());
    let log_sync = log_sync_worker(&config.bind, &config.src.prefix, tx.clone())?;
    add_audit_device(&device_name, &config.external_address, &shared_src_client.clone().lock().unwrap())?;
    ctrlc_handler(&device_name, shared_src_client.clone())?;

    let full_sync = full_sync_worker(&config, shared_src_client.clone(), tx.clone());

    let _ = (sync.join(), log_sync.join(), full_sync.join(), src_token.join(), dst_token.join());

    Ok(())
}

fn load_config(file_name: &str) -> Result<VaultSyncConfig, Box<dyn Error>> {
    match VaultSyncConfig::from_file(file_name) {
        Ok(config) => {
            info!("Configuration from {}:\n{}", file_name, serde_json::to_string_pretty(&config).unwrap());
            Ok(config)
        },
        Err(error) => {
            error!("Failed to load configuration file {}: {}", file_name, error);
            Err(error)
        }
    }
}

fn vault_client(host: &VaultHost) -> Result<VaultClient, Box<dyn Error>> {
    match vault::vault_client(host) {
        Ok(client) => {
            Ok(client)
        },
        Err(error) => {
            error!("Failed to connect to {}: {}", &host.url, error);
            Err(error.into())
        }
    }
}

fn token_worker(host: &VaultHost, client: Arc<Mutex<VaultClient>>) -> thread::JoinHandle<()> {
    let host = host.clone();
    thread::spawn(move || {
        vault::token_worker(&host, client);
    })
}

fn sync_worker(
    rx: mpsc::Receiver<sync::SecretOp>,
    src_prefix: &str,
    dst_prefix: &str,
    src_client: Arc<Mutex<VaultClient>>,
    dst_client: Arc<Mutex<VaultClient>>,
    dry_run: bool,
) -> thread::JoinHandle<()> {
    info!("Dry run: {}", dry_run);
    let src_prefix = src_prefix.to_string();
    let dst_prefix = dst_prefix.to_string();
    thread::spawn(move || {
        sync::sync_worker(rx, &src_prefix, &dst_prefix, src_client, dst_client, dry_run);
    })
}

fn delete_audit_device(device_name: &str, client: &VaultClient) {
    sync::audit_device_delete(device_name, client);
}

fn add_audit_device(device_name: &str, external_address: &str, client: &VaultClient) -> VaultResult<()> {
    if let Err(error) = sync::audit_device_add(device_name, external_address, client) {
        error!("Failed to add Vault audit device: {}", error);
        return Err(error.into());
    }
    info!("Audit device vault-sync exists: {}", sync::audit_device_exists(device_name, client));
    Ok(())
}

fn log_sync_worker(addr: &str, prefix: &str, tx: mpsc::Sender<sync::SecretOp>) -> Result<thread::JoinHandle<()>, std::io::Error> {
    let prefix = prefix.to_string();
    info!("Listening on {}", addr);
    let listener = TcpListener::bind(addr)?;
    let handle = thread::spawn(move || {
        for stream in listener.incoming() {
            if let Ok(stream) = stream {
                let tx = tx.clone();
                let prefix = prefix.clone();
                thread::spawn(move || {
                    sync::log_sync(&prefix, stream, tx);
                });
            }
        }
    });
    Ok(handle)
}

fn full_sync_worker(
    config: &VaultSyncConfig,
    client: Arc<Mutex<VaultClient>>,
    tx: mpsc::Sender<sync::SecretOp>
) -> thread::JoinHandle<()>{
    let interval = time::Duration::from_secs(config.full_sync_interval);
    let prefix = config.src.prefix.clone();
    thread::spawn(move || {
        sync::full_sync_worker(&prefix, interval, client, tx);
    })
}

fn ctrlc_handler(device_name: &str, client: Arc<Mutex<VaultClient>>) -> Result<(), Box<dyn Error>> {
    let device_name = device_name.to_string();
    match ctrlc::set_handler(move || {
        info!("Shutting down");
        let client = client.lock().unwrap();
        delete_audit_device(&device_name, &client);
        std::process::exit(0);
    }) {
        Ok(()) => Ok(()),
        Err(error) => Err(error.into())
    }
}