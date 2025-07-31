mod context;
mod fat;
mod patch;
mod thin;

use std::net::TcpListener;
use std::sync::Arc;
use std::sync::mpsc::{Sender, channel};
use std::time::Duration;
use std::{path::PathBuf, process::Command};

use clap::Parser;
use context::Context;
use dioxus_devtools::DevserverMsg;
use patch::{HotpatchModuleCache, create_jump_table};
use serde::{Deserialize, Serialize};
use target_lexicon::Triple;
use tempfile::NamedTempFile;
use tungstenite::handshake::server::{Request, Response};

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

fn ws_server(receiver: multiqueue::BroadcastReceiver<DevserverMsg>, aslr_tx: Sender<u64>) {
    let server = TcpListener::bind("127.0.0.1:3100").unwrap();
    // TODO?: send accumulated patches to newly connected clients
    // we don't modify original binary so if you run binary a second time then it wont have
    // patches that been made between first and second launches
    //
    // UPD: now it uses `multiqueue` crate which provides broadcast spmc/mpmc channels
    // and using `add_stream` seems to achieve that behaviour, sending previously built patches
    // TODO: if we need to do a fat rebuild, then we need to clean previous patches
    for new_stream in server.incoming() {
        if let Ok(stream) = new_stream {
            let channel = receiver.add_stream();
            let aslr_tx_clone = aslr_tx.clone();
            std::thread::spawn(move || {
                let mut websocket =
                    tungstenite::accept_hdr(stream, |request: &Request, response: Response| {
                        if let Some(query) = request.uri().query() {
                            let split = query.split("&");
                            // very ugly and bad hack to get aslr of a executable back
                            // TODO: find another way to get aslr reference back
                            for s in split {
                                if let Some(aslr_str) = s.strip_prefix("aslr_reference=") {
                                    if let Ok(aslr_reference) = aslr_str.parse() {
                                        aslr_tx_clone.send(aslr_reference).unwrap();
                                    }
                                    break;
                                }
                            }
                        }
                        Ok(Response::from(response))
                    })
                    .unwrap();
                println!("WS connected");

                loop {
                    if !websocket.can_write() {
                        break;
                    }
                    if let Ok(msg) = channel.try_recv() {
                        let serialized = serde_json::to_string(&msg).unwrap();
                        websocket
                            .send(tungstenite::Message::Text(serialized.into()))
                            .unwrap();
                    }
                    std::thread::sleep(Duration::from_millis(50));
                }
                println!("WS loop exited");
            });
        }
    }
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
    std::thread::spawn(|| ws_server(rx, aslr_tx));

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

    let exe_path = fat::build_fat(&ctx);

    let rustc_args: RustcArgs =
        serde_json::from_str(&std::fs::read_to_string(ctx.rustc_wrapper_file.path()).unwrap())
            .unwrap();

    let cache = Arc::new(HotpatchModuleCache::new(&exe_path, &ctx.triple).unwrap());
    let mut pid = None;
    let mut exe = None;
    if ctx.bin.is_some() {
        let mut exe_cmd = Command::new(exe_path);
        let new_exe = exe_cmd.spawn().unwrap();
        pid = Some(new_exe.id());
        exe = Some(new_exe);
    }

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
                println!("RELOADING");
                let time_start = thin::build_thin(&ctx, &rustc_args, aslr_reference, &cache);

                let new = ctx.patch_exe(time_start);
                // tracing::debug!("Patching {} -> {}", "", new.display());
                let mut jump_table = create_jump_table(&new, &ctx.triple, &cache).unwrap();

                if ctx.triple.architecture == target_lexicon::Architecture::Wasm32 {
                    // Make sure we use the dir relative to the public dir, so the web can load it as a proper URL
                    //
                    // ie we would've shipped `/Users/foo/Projects/dioxus/target/dx/project/debug/web/public/wasm/lib.wasm`
                    //    but we want to ship `/wasm/lib.wasm`
                    let patch_lib_name = jump_table.lib.file_name().unwrap();
                    ctx.write_thin_wasm_patch_to_pkg(&jump_table.lib);
                    jump_table.lib = PathBuf::from("/pkg/").join(patch_lib_name);
                }

                let msg = DevserverMsg::HotReload(dioxus_devtools::HotReloadMsg {
                    templates: Vec::new(),
                    assets: Vec::new(),
                    ms_elapsed: 0,
                    jump_table: Some(jump_table),
                    for_build_id: None,
                    for_pid: pid,
                });
                tx.try_send(msg).unwrap();
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
