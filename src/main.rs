mod builder;
mod context;
mod fat;
mod patch;
mod thin;
mod ws_server;

use std::sync::Arc;
use std::sync::mpsc::channel;
use std::{path::PathBuf, sync::atomic::AtomicU64};

use clap::Parser;
use context::Context;
use serde::{Deserialize, Serialize};
use target_lexicon::Triple;
use tempfile::NamedTempFile;
use ws_server::HotPatchServer;

#[derive(clap::Parser)]
struct Args {
    #[clap(long)]
    manifest_path: PathBuf,
    #[clap(long)]
    bin: Option<String>,
    #[clap(long)]
    lib: bool,
    #[clap(short, long, default_value = "true")]
    thin: bool,
    #[clap(long, default_value = "unknown-unknown-unknown")]
    target: Triple,
    #[clap(long)]
    package: String,
    #[clap(long)]
    features: Vec<String>,
    #[clap(long)]
    rust_flags: Vec<String>,
    #[clap(long, default_value = "false")]
    no_default_features: bool,
}

#[derive(Default, Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct RustcArgs {
    pub args: Vec<String>,
    pub envs: Vec<(String, String)>,
    pub link_args: Vec<String>,
}

#[derive(Debug, Serialize, Deserialize, PartialEq, Clone)]
pub enum LinkerFlavor {
    Gnu,
    Darwin,
    WasmLld,
    Msvc,
    Unsupported, // a catch-all for unsupported linkers, usually the stripped-down unix ones
}

fn main() {
    tracing_subscriber::fmt::init();

    let mut args = Args::parse();
    if args.target == Triple::unknown() {
        args.target = Triple::host();
    }
    let manifest = args.manifest_path.canonicalize().unwrap();
    let mut working_dir = manifest.clone();
    working_dir.pop();
    let target_dir = working_dir.join("target");
    let bundle_path = target_dir.join("bundle");
    std::fs::create_dir_all(&bundle_path).unwrap();

    let rustc_wrapper_file = NamedTempFile::with_suffix(".json").unwrap();
    let link_args_file = NamedTempFile::with_suffix(".txt").unwrap();
    let link_err_file = NamedTempFile::with_suffix(".txt").unwrap();

    let aslr_reference = Arc::new(AtomicU64::new(0));

    let (clear_patches_tx, clear_patches_rx) = channel::<()>();

    // TODO: reduce capacity of a queue
    // but to do that we need to either read from rx or drop it in websocket loop,
    // but we need to add streams to it when new client connects
    // so idk how to do it right now, I wrote websocket code very poorly
    let (tx, rx) = multiqueue::broadcast_queue(100);
    let mut hp_server = HotPatchServer::new(
        "127.0.0.1:3100",
        rx,
        Arc::clone(&aslr_reference),
        clear_patches_rx,
    );
    std::thread::spawn(move || hp_server.run());

    let ctx = Context {
        target_dir,
        working_dir,
        bin: args.bin,
        lib: args.lib,
        triple: args.target.clone(),
        features: args.features,
        rustc_wrapper_file,
        link_args_file,
        link_err_file,
        bundle_path,
        profile_name: "dev".to_string(),
        profile_dir: "debug".to_string(),
        package: args.package,
        rust_flags: args.rust_flags,
        no_default_features: args.no_default_features,
        site_dir: "target/site".to_string(),
        site_pkg_dir: "pkg".to_string(),
        wasm_bindgen_dir: "wasm-bindgen".to_string(),
    };

    let (command_tx, command_rx) = channel();

    let mut builder = builder::Builder::new(ctx, tx, aslr_reference, command_rx);
    std::thread::spawn(move || builder.run());

    command_tx.send(builder::BuildCommand::Fat).unwrap();

    let mut line = String::new();
    loop {
        line.clear();
        std::io::stdin().read_line(&mut line).unwrap();
        match line.as_str().trim() {
            "r" => {
                command_tx.send(builder::BuildCommand::Thin).unwrap();
            }
            "R" => {
                clear_patches_tx.send(()).unwrap();
                command_tx.send(builder::BuildCommand::FatRebuild).unwrap();
            }
            "e" => {
                println!("EXITING");
                break;
            }
            _ => (),
        }
    }
}
