mod builder;
mod context;
mod fat;
mod patch;
mod thin;
mod ws_server;

use std::path::PathBuf;
use std::sync::mpsc::channel;
use std::time::Duration;

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
    #[clap(long, default_value = "Triple::host()")]
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

    let args = Args::parse();
    let manifest = args.manifest_path.canonicalize().unwrap();
    let mut working_dir = manifest.clone();
    working_dir.pop();
    let target_dir = working_dir.join("target");
    let bundle_path = target_dir.join("bundle");
    std::fs::create_dir_all(&bundle_path).unwrap();

    let rustc_wrapper_file = NamedTempFile::with_suffix(".json").unwrap();
    dbg!(rustc_wrapper_file.path());
    let link_args_file = NamedTempFile::with_suffix(".txt").unwrap();
    dbg!(link_args_file.path());
    let link_err_file = NamedTempFile::with_suffix(".txt").unwrap();
    dbg!(link_err_file.path());

    let (tx, rx) = multiqueue::broadcast_queue(10);
    let (aslr_tx, aslr_rx) = channel();
    let hp_server = HotPatchServer::new("127.0.0.1:3100", rx, aslr_tx);
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

    let mut builder = builder::Builder::new(ctx, tx);

    let exe_path = builder.build_fat();
    let exe = builder.run_if_native(&exe_path);

    let mut aslr_reference = 0;
    if let Ok(aslr) = aslr_rx.recv_timeout(Duration::from_secs(1)) {
        aslr_reference = aslr;
    } else {
        dbg!("aslr timeout");
    }
    dbg!(aslr_reference);

    let mut line = String::new();
    loop {
        line.clear();
        std::io::stdin().read_line(&mut line).unwrap();
        match line.as_str().trim() {
            "r" => {
                builder.build_thin();
            }
            "e" => {
                println!("EXITING");
                break;
            }
            _ => (),
        }
    }
    if let Some(mut exe) = exe {
        exe.kill().unwrap();
    }
}
