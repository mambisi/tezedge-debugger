#![allow(dead_code)]

mod configuration;
mod actors;
mod network;
mod storage;

use std::{
    process::Command,
    sync::{Mutex, Arc},
};

use failure::{Error, Fail};
use riker::actors::*;
use warp::{
    Filter,
    http::Response,
};

use crate::{
    actors::prelude::*,
    network::prelude::*,
    configuration::AppConfig,
};

#[derive(Debug, Fail)]
enum AppError {
    #[fail(display = "no valid network interface found")]
    NoNetworkInterface,
    #[fail(display = "only ethernet channels supported for now")]
    UnsupportedNetworkChannelType,
    #[fail(display = "encountered io error: {}", _0)]
    IOError(std::io::Error),
    #[fail(display = "received invalid packet")]
    InvalidPacket,
}

fn set_sysctl(ifaces: &[&str]) {
    for iface in ifaces {
        Command::new("sysctl")
            .args(&["-w", &format!("net.ipv4.conf.{}.rp_filter=2", iface)])
            .output().unwrap();
    }
    Command::new("sysctl")
        .args(&["-w", "net.ipv4.ip_forward=1"])
        .output().unwrap();
}

#[tokio::main]
async fn main() -> Result<(), Error> {
    // -- Initialize logger
    simple_logger::init()?;

    // -- Load basic arguments
    let app_config = AppConfig::from_env();
    log::info!("Loaded arguments from CLI");
    let identity = app_config.load_identity()?;
    log::info!("Loaded identity file from '{}'", app_config.identity_file);

    // -- Initialize RocksDB
    let db = app_config.open_database()?;
    log::info!("Created RocksDB storage in: {}", app_config.storage_path);

    // -- Create TUN devices
    let ((_, receiver), writer) = make_bridge(
        &app_config.tun0_address_space,
        &app_config.tun1_address_space,
        &app_config.tun0_address,
        &app_config.tun1_address,
        app_config.local_address.parse()?,
        app_config.tun1_address.parse()?,
    )?;

    log::info!("Created TUN bridge on {} <-> {} <-> {}",
        app_config.local_address,
        app_config.tun0_address,
        app_config.tun1_address,
    );

    // -- Setup redirects
    // Command::new("ip")
    //     .args(&["rule", "add", "fwmark", "1", "table", "1"])
    //     .output().unwrap();
    // Command::new("ip")
    //     .args(&["route", "add", "default", "dev", &app_config.tun0_name, "table", "1"])
    //     .output().unwrap();
    // Command::new("iptables")
    //     .args(&["-t", "mangle", "-A", "OUTPUT",
    //         "--source", &app_config.local_address,
    //         "-o", &app_config.interface, "-p", "tcp",
    //         "--dport", &app_config.port.to_string(),
    //         "-j", "MARK", "--set-mark", "1"])
    //     .output().unwrap();
    Command::new("iptables")
        .args(&["-t", "nat", "-A", "POSTROUTING",
            "--source", &app_config.tun1_address_space,
            "-o", &app_config.interface, "-j", "MASQUERADE"])
        .output().unwrap();
    set_sysctl(&["all", "default", &app_config.tun0_name, &app_config.tun1_name, &app_config.interface]);


    // -- Start Actor system
    let system = ActorSystem::new()?;
    let orchestrator = system.actor_of(Props::new_args(PacketOrchestrator::new, PacketOrchestratorArgs {
        local_address: app_config.local_address.parse()?,
        fake_address: app_config.tun1_address.parse()?,
        local_identity: identity.clone(),
        db: db.clone(),
        writer: Arc::new(Mutex::new(writer)),
    }), "packet_orchestrator")?;

    std::thread::spawn(move || {
        loop {
            for message in receiver.recv() {
                orchestrator.tell(message, None);
            }
        }
    });

    log::info!("Starting to analyze traffic on port {}", app_config.port);

    let cloner = move || {
        db.clone()
    };

    // -- Initialize server
    let endpoint = warp::path!("data" / u64 / u64)
        .map(move |start, end| {
            use storage::rpc_message::RpcMessage;
            match cloner().get_range(start, end) {
                Ok(value) => {
                    let value: Vec<RpcMessage> = value.into_iter()
                        .map(|x| RpcMessage::from(x)).collect();
                    serde_json::to_string(&value).expect("failed to serialize the array")
                }
                Err(e) => serde_json::to_string(&
                    format!("Failed to read database: {}", e)
                ).unwrap()
            }
        })
        .map(|value| {
            Response::builder()
                .header("Content-Type", "application/json")
                .body(value)
        });

    warp::serve(endpoint)
        // TODO: Add as config settings
        .run(([127, 0, 0, 1], 5050))
        .await;

    Ok(())
}