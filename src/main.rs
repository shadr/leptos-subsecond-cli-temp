mod builder;
mod context;
mod fat;
mod patch;
mod thin;
mod ws_server;

use std::path::Path;
use std::str::FromStr;
use std::sync::Arc;
use std::sync::mpsc::{Receiver, Sender, channel};
use std::{path::PathBuf, sync::atomic::AtomicU64};

use builder::BuildCommand;
use clap::Parser;
use context::Context;
use dioxus_devtools::DevserverMsg;
use multiqueue::BroadcastSender;
use serde::{Deserialize, Serialize};
use target_lexicon::Triple;
use tempfile::NamedTempFile;
use ws_server::HotPatchServer;

#[derive(clap::Parser)]
struct Args {
    #[clap(long)]
    manifest_path: PathBuf,
    #[clap(subcommand)]
    command: Command,
}

#[derive(clap::Parser)]
enum Command {
    Raw(RawArgs),
    Leptos(LeptosArgs),
}

#[derive(clap::Parser)]
struct LeptosArgs {
    #[clap(long, default_value = "unknown-unknown-unknown")]
    target: Triple,
    #[clap(long)]
    package: String,

    #[clap(long)]
    server_bin: String,
    #[clap(long)]
    server_rust_flags: Vec<String>,
    #[clap(long, default_value = "false")]
    server_no_default_features: bool,
    #[clap(long)]
    server_features: Vec<String>,

    #[clap(long)]
    lib_rust_flags: Vec<String>,
    #[clap(long, default_value = "false")]
    lib_no_default_features: bool,
    #[clap(long)]
    lib_features: Vec<String>,
}

#[derive(clap::Parser)]
struct RawArgs {
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

    let manifest = args.manifest_path.canonicalize().unwrap();
    let mut working_dir = manifest.clone();
    working_dir.pop();
    let target_dir = working_dir.join("target");
    let bundle_path = target_dir.join("bundle");
    std::fs::create_dir_all(&bundle_path).unwrap();

    let aslr_reference = Arc::new(AtomicU64::new(0));

    let (clear_patches_tx, tx) = spawn_hotpatch_server(Arc::clone(&aslr_reference));
    let (back_command_tx, back_command_rx) = channel();
    let (front_command_tx, front_command_rx) = channel();

    let mut has_front = false;

    match args.command {
        Command::Raw(mut raw_args) => {
            if raw_args.target == Triple::unknown() {
                raw_args.target = Triple::host();
            }
            spawn_raw_builder(
                &args.manifest_path,
                &raw_args,
                tx,
                aslr_reference,
                back_command_rx,
            );
        }
        Command::Leptos(mut leptos_args) => {
            has_front = true;
            spawn_backend_builder(
                &args.manifest_path,
                &mut leptos_args,
                tx.clone(),
                Arc::clone(&aslr_reference),
                back_command_rx,
            );

            spawn_frontend_builder(
                &args.manifest_path,
                &mut leptos_args,
                tx,
                aslr_reference,
                front_command_rx,
            );
        }
    }

    back_command_tx.send(builder::BuildCommand::Fat).unwrap();
    if has_front {
        front_command_tx.send(BuildCommand::Fat).unwrap();
    }

    let mut line = String::new();
    loop {
        line.clear();
        std::io::stdin().read_line(&mut line).unwrap();
        match line.as_str().trim() {
            "r" => {
                back_command_tx.send(builder::BuildCommand::Thin).unwrap();
                if has_front {
                    front_command_tx.send(BuildCommand::Thin).unwrap();
                }
            }
            "R" => {
                clear_patches_tx.send(()).unwrap();
                back_command_tx
                    .send(builder::BuildCommand::FatRebuild)
                    .unwrap();
                if has_front {
                    front_command_tx.send(BuildCommand::FatRebuild).unwrap();
                }
            }
            "e" => {
                println!("EXITING");
                break;
            }
            _ => (),
        }
    }
}

fn spawn_hotpatch_server(
    aslr_reference: Arc<AtomicU64>,
) -> (Sender<()>, BroadcastSender<DevserverMsg>) {
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
    (clear_patches_tx, tx)
}

fn spawn_raw_builder(
    manifest_path: &Path,
    args: &RawArgs,
    patch_sender: BroadcastSender<DevserverMsg>,
    aslr_reference: Arc<AtomicU64>,
    command_rx: Receiver<BuildCommand>,
) {
    let manifest = manifest_path.canonicalize().unwrap();
    let mut working_dir = manifest.clone();
    working_dir.pop();

    let target_dir = working_dir.join("target");
    let bundle_path = target_dir.join("bundle");

    let rustc_wrapper_file = NamedTempFile::with_suffix(".json").unwrap();
    let link_args_file = NamedTempFile::with_suffix(".txt").unwrap();
    let link_err_file = NamedTempFile::with_suffix(".txt").unwrap();

    let ctx = Context {
        target_dir,
        working_dir,
        bin: args.bin.clone(),
        lib: args.lib,
        triple: args.target.clone(),
        features: args.features.clone(),
        rustc_wrapper_file,
        link_args_file,
        link_err_file,
        bundle_path,
        profile_name: "dev".to_string(),
        profile_dir: "debug".to_string(),
        package: args.package.clone(),
        rust_flags: args.rust_flags.clone(),
        no_default_features: args.no_default_features,
        site_dir: "target/site".to_string(),
        site_pkg_dir: "pkg".to_string(),
        wasm_bindgen_dir: "wasm-bindgen".to_string(),
    };

    let mut builder = builder::Builder::new(ctx, patch_sender, aslr_reference, command_rx);
    std::thread::spawn(move || builder.run());
}

fn spawn_backend_builder(
    manifest_path: &Path,
    args: &mut LeptosArgs,
    patch_sender: BroadcastSender<DevserverMsg>,
    aslr_reference: Arc<AtomicU64>,
    command_rx: Receiver<BuildCommand>,
) {
    let manifest = manifest_path.canonicalize().unwrap();
    let mut working_dir = manifest.clone();
    working_dir.pop();

    let target_dir = working_dir.join("target");
    let bundle_path = target_dir.join("bundle");

    let rustc_wrapper_file = NamedTempFile::with_suffix(".json").unwrap();
    let link_args_file = NamedTempFile::with_suffix(".txt").unwrap();
    let link_err_file = NamedTempFile::with_suffix(".txt").unwrap();

    args.server_rust_flags
        .push("cfg erase_components".to_string());

    let ctx = Context {
        target_dir,
        working_dir,
        bin: Some(args.server_bin.clone()),
        lib: false,
        triple: Triple::host(),
        features: args.server_features.clone(),
        rustc_wrapper_file,
        link_args_file,
        link_err_file,
        bundle_path,
        profile_name: "dev".to_string(),
        profile_dir: "debug".to_string(),
        package: args.package.clone(),
        rust_flags: args.server_rust_flags.clone(),
        no_default_features: args.server_no_default_features,
        site_dir: "target/site".to_string(),
        site_pkg_dir: "pkg".to_string(),
        wasm_bindgen_dir: "wasm-bindgen".to_string(),
    };

    let mut builder = builder::Builder::new(ctx, patch_sender, aslr_reference, command_rx);
    std::thread::spawn(move || builder.run());
}

fn spawn_frontend_builder(
    manifest_path: &Path,
    args: &mut LeptosArgs,
    patch_sender: BroadcastSender<DevserverMsg>,
    aslr_reference: Arc<AtomicU64>,
    command_rx: Receiver<BuildCommand>,
) {
    let manifest = manifest_path.canonicalize().unwrap();
    let mut working_dir = manifest.clone();
    working_dir.pop();

    let target_dir = working_dir.join("target");
    let bundle_path = target_dir.join("bundle");

    let rustc_wrapper_file = NamedTempFile::with_suffix(".json").unwrap();
    let link_args_file = NamedTempFile::with_suffix(".txt").unwrap();
    let link_err_file = NamedTempFile::with_suffix(".txt").unwrap();

    args.lib_rust_flags
        .push("cfg getrandom_backend=\"wasm_js\"".to_string());
    args.lib_rust_flags.push("cfg erase_components".to_string());

    let ctx = Context {
        target_dir,
        working_dir,
        bin: None,
        lib: true,
        triple: Triple::from_str("wasm32-unknown-unknown").unwrap(),
        features: args.lib_features.clone(),
        rustc_wrapper_file,
        link_args_file,
        link_err_file,
        bundle_path,
        profile_name: "dev".to_string(),
        profile_dir: "debug".to_string(),
        package: args.package.clone(),
        rust_flags: args.lib_rust_flags.clone(),
        no_default_features: args.lib_no_default_features,
        site_dir: "target/site".to_string(),
        site_pkg_dir: "pkg".to_string(),
        wasm_bindgen_dir: "wasm-bindgen".to_string(),
    };

    let mut builder = builder::Builder::new(ctx, patch_sender, aslr_reference, command_rx);
    std::thread::spawn(move || builder.run());
}
