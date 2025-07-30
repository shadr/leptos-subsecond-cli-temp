mod patch;

use std::ffi::OsString;
use std::net::TcpListener;
use std::path::Path;
use std::sync::Arc;
use std::sync::mpsc::{Receiver, Sender, channel};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use std::{path::PathBuf, process::Command};

use clap::Parser;
use dioxus_devtools::DevserverMsg;
use itertools::Itertools;
use patch::{
    HotpatchModuleCache, create_jump_table, create_undefined_symbol_stub, prepare_wasm_base_module,
};
use serde::{Deserialize, Serialize};
use target_lexicon::{OperatingSystem, Triple};
use tempfile::NamedTempFile;
use tungstenite::handshake::server::{Request, Response};
use uuid::Uuid;
use wasm_bindgen_cli_support::Bindgen;

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

fn ws_server(channel: Receiver<DevserverMsg>, aslr_tx: Sender<u64>) {
    let server = TcpListener::bind("127.0.0.1:3100").unwrap();
    let stream = server.incoming().next().unwrap().unwrap();
    println!("WS connected");
    let mut websocket = tungstenite::accept_hdr(stream, |request: &Request, response: Response| {
        let split = request.uri().query().unwrap().split("&");
        for s in split {
            if let Some(aslr_str) = s.strip_prefix("aslr_reference=") {
                let aslr_reference: u64 = aslr_str.parse().unwrap();
                aslr_tx.send(aslr_reference).unwrap();
            }
        }
        Ok(Response::from(response))
    })
    .unwrap();
    while let Ok(msg) = channel.recv() {
        let serialized = serde_json::to_string(&msg).unwrap();
        websocket
            .send(tungstenite::Message::Text(serialized.into()))
            .unwrap();
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

    let (tx, rx) = channel();
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
        linker_flavor: match args.target.environment {
            target_lexicon::Environment::Gnu
            | target_lexicon::Environment::Gnuabi64
            | target_lexicon::Environment::Gnueabi
            | target_lexicon::Environment::Gnueabihf
            | target_lexicon::Environment::GnuLlvm => LinkerFlavor::Gnu,
            _ => match args.target.operating_system {
                OperatingSystem::Linux => LinkerFlavor::Gnu,
                _ => match args.target.architecture {
                    target_lexicon::Architecture::Wasm32 | target_lexicon::Architecture::Wasm64 => {
                        LinkerFlavor::WasmLld
                    }
                    _ => LinkerFlavor::Unsupported,
                },
            },
        },
        rust_flags: args.rust_flags,
        no_default_features: args.no_default_features,
    };

    let exe_path = build_fat(&ctx);

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
                let time_start = build_thin(&ctx, &rustc_args, aslr_reference, &cache);

                let new = patch_exe(&ctx, time_start);
                dbg!(&new);
                tracing::debug!("Patching {} -> {}", "", new.display());
                let mut jump_table = create_jump_table(&new, &ctx.triple, &cache).unwrap();

                if ctx.triple.architecture == target_lexicon::Architecture::Wasm32 {
                    // Make sure we use the dir relative to the public dir, so the web can load it as a proper URL
                    //
                    // ie we would've shipped `/Users/foo/Projects/dioxus/target/dx/project/debug/web/public/wasm/lib.wasm`
                    //    but we want to ship `/wasm/lib.wasm`
                    let patch_lib_name = jump_table.lib.file_name().unwrap();
                    std::fs::copy(
                        &jump_table.lib,
                        ctx.target_dir.join("site/pkg").join(patch_lib_name),
                    )
                    .unwrap();
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
                tx.send(msg).unwrap();
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

struct Context {
    working_dir: PathBuf,
    target_dir: PathBuf,
    bin: Option<String>,
    lib: bool,
    triple: Triple,
    rustc_wrapper_file: NamedTempFile,
    link_args_file: NamedTempFile,
    link_err_file: NamedTempFile,
    bundle_path: PathBuf,
    profile_dir: String,
    profile_name: String,
    package: String,
    linker_flavor: LinkerFlavor,
    features: Vec<String>,
    rust_flags: Vec<String>,
    no_default_features: bool,
}

impl Context {
    pub fn frameworks_directory(&self) -> PathBuf {
        self.target_dir.join("frameworks")
    }

    fn select_linker(&self) -> PathBuf {
        match self.linker_flavor {
            LinkerFlavor::WasmLld => PathBuf::from("wasm-ld"),
            LinkerFlavor::Gnu => PathBuf::from("cc"),
            _ => PathBuf::from("cc"),
        }
    }

    fn is_wasm_or_wasi(&self) -> bool {
        matches!(
            self.triple.architecture,
            target_lexicon::Architecture::Wasm32 | target_lexicon::Architecture::Wasm64
        ) || self.triple.operating_system == target_lexicon::OperatingSystem::Wasi
    }

    fn final_binary_name(&self) -> String {
        if let Some(bin_name) = &self.bin {
            return bin_name.clone();
        }
        if self.lib {
            return self.package.clone();
        }
        unreachable!("you should specify either bin {{name}} or lib");
    }
}

fn write_executable(ctx: &Context, compiled: &Path) -> PathBuf {
    if ctx.is_wasm_or_wasi() {
        let wasm_bindgen_dir = ctx.target_dir.join("wasm-bindgen/");
        std::fs::remove_dir_all(&wasm_bindgen_dir).unwrap();
        std::fs::create_dir_all(&wasm_bindgen_dir).unwrap();

        tracing::info!("preparing wasm file for bindgen");
        let unprocessed = std::fs::read(compiled).unwrap();
        let all_exported_bytes = prepare_wasm_base_module(&unprocessed).unwrap();
        std::fs::write(compiled, all_exported_bytes).unwrap();
        tracing::info!("preparing wasm file done");

        tracing::info!("running wasm-bindgen");
        let mut bindgen = Bindgen::new()
            .keep_lld_exports(true)
            .demangle(false) // do no demangle names, hotpatchmodulecache ifunc map not populated properly with demangled names for some reason
            .debug(true)
            .keep_debug(true)
            .input_path(compiled)
            .out_name(&ctx.final_binary_name())
            .web(true)
            .unwrap()
            .generate_output()
            .unwrap();

        bindgen.emit(&wasm_bindgen_dir).unwrap();
        tracing::info!("wasm-bindgen done");

        let new_wasm_path = wasm_bindgen_dir
            .join(ctx.final_binary_name())
            .with_extension("wasm");

        std::fs::rename(
            wasm_bindgen_dir.join(format!("{}_bg.wasm", ctx.final_binary_name())),
            &new_wasm_path,
        )
        .unwrap();

        std::fs::copy(
            &new_wasm_path,
            ctx.target_dir
                .join("site/pkg")
                .join(new_wasm_path.file_name().unwrap()),
        )
        .unwrap();

        let js_path = new_wasm_path.with_file_name(format!("{}.js", ctx.final_binary_name()));

        std::fs::copy(
            &js_path,
            ctx.target_dir
                .join("site/pkg")
                .join(js_path.file_name().unwrap()),
        )
        .unwrap();

        new_wasm_path
    } else {
        let bundle_exe = ctx.bundle_path.join(&ctx.final_binary_name());
        std::fs::copy(&compiled, &bundle_exe).unwrap();
        bundle_exe
    }
}

fn patch_exe(ctx: &Context, time_start: SystemTime) -> PathBuf {
    let compiled_exe = ctx
        .target_dir
        .join(ctx.triple.to_string())
        .join(&ctx.profile_dir)
        .join(ctx.final_binary_name());
    let path = compiled_exe.with_file_name(format!(
        "lib{}-patch-{}",
        ctx.final_binary_name(),
        time_start
            .duration_since(UNIX_EPOCH)
            .map(|f| f.as_millis())
            .unwrap_or(0),
    ));

    let extension = match ctx.linker_flavor {
        LinkerFlavor::Darwin => "dylib",
        LinkerFlavor::Gnu => "so",
        LinkerFlavor::WasmLld => "wasm",
        LinkerFlavor::Msvc => "dll",
        LinkerFlavor::Unsupported => "",
    };

    path.with_extension(extension)
}

fn thin_link_args(ctx: &Context, original_args: &[&str]) -> Vec<String> {
    let mut out_args = vec![];

    match ctx.linker_flavor {
        // wasm32-unknown-unknown -> use wasm-ld (gnu-lld)
        //
        // We need to import a few things - namely the memory and ifunc table.
        //
        // We can safely export everything, I believe, though that led to issues with the "fat"
        // binaries that also might lead to issues here too. wasm-bindgen chokes on some symbols
        // and the resulting JS has issues.
        //
        // We turn on both --pie and --experimental-pic but I think we only need --pie.
        //
        // We don't use *any* of the original linker args since they do lots of custom exports
        // and other things that we don't need.
        //
        // The trickiest one here is -Crelocation-model=pic, which forces data symbols
        // into a GOT, making it possible to import them from the main module.
        //
        // I think we can make relocation-model=pic work for non-wasm platforms, enabling
        // fully relocatable modules with no host coordination in lieu of sending out
        // the aslr slide at runtime.
        LinkerFlavor::WasmLld => {
            out_args.extend([
                "--fatal-warnings".to_string(),
                "--verbose".to_string(),
                "--import-memory".to_string(),
                "--import-table".to_string(),
                "--growable-table".to_string(),
                "--export".to_string(),
                "main".to_string(),
                "--allow-undefined".to_string(),
                "--no-demangle".to_string(),
                "--no-entry".to_string(),
                "--pie".to_string(),
                "--experimental-pic".to_string(),
            ]);

            // retain exports so post-processing has hooks to work with
            for (idx, arg) in original_args.iter().enumerate() {
                if *arg == "--export" {
                    out_args.push(arg.to_string());
                    out_args.push(original_args[idx + 1].to_string());
                }
            }
        }

        // This uses "cc" and these args need to be ld compatible
        //
        // Most importantly, we want to pass `-dylib` to both CC and the linker to indicate that
        // we want to generate the shared library instead of an executable.
        LinkerFlavor::Darwin => {
            out_args.extend(["-Wl,-dylib".to_string()]);

            // Preserve the original args. We only preserve:
            // -framework
            // -arch
            // -lxyz
            // There might be more, but some flags might break our setup.
            for (idx, arg) in original_args.iter().enumerate() {
                if *arg == "-framework" || *arg == "-arch" || *arg == "-L" {
                    out_args.push(arg.to_string());
                    out_args.push(original_args[idx + 1].to_string());
                }

                if arg.starts_with("-l") || arg.starts_with("-m") {
                    out_args.push(arg.to_string());
                }
            }
        }

        // android/linux need to be compatible with lld
        //
        // android currently drags along its own libraries and other zany flags
        LinkerFlavor::Gnu => {
            out_args.extend([
                "-shared".to_string(),
                "-Wl,--eh-frame-hdr".to_string(),
                "-Wl,-z,noexecstack".to_string(),
                "-Wl,-z,relro,-z,now".to_string(),
                "-nodefaultlibs".to_string(),
                "-Wl,-Bdynamic".to_string(),
            ]);

            // Preserve the original args. We only preserve:
            // -L <path>
            // -arch
            // -lxyz
            // There might be more, but some flags might break our setup.
            for (idx, arg) in original_args.iter().enumerate() {
                if *arg == "-L" {
                    out_args.push(arg.to_string());
                    out_args.push(original_args[idx + 1].to_string());
                }

                if arg.starts_with("-l")
                    || arg.starts_with("-m")
                    || arg.starts_with("-Wl,--target=")
                    || arg.starts_with("-Wl,-fuse-ld")
                    || arg.starts_with("-fuse-ld")
                    || arg.contains("-ld-path")
                {
                    out_args.push(arg.to_string());
                }
            }
        }

        LinkerFlavor::Msvc => {
            out_args.extend([
                "shlwapi.lib".to_string(),
                "kernel32.lib".to_string(),
                "advapi32.lib".to_string(),
                "ntdll.lib".to_string(),
                "userenv.lib".to_string(),
                "ws2_32.lib".to_string(),
                "dbghelp.lib".to_string(),
                "/defaultlib:msvcrt".to_string(),
                "/DLL".to_string(),
                "/DEBUG".to_string(),
                "/PDBALTPATH:%_PDB%".to_string(),
                "/EXPORT:main".to_string(),
                "/HIGHENTROPYVA:NO".to_string(),
            ]);
        }

        LinkerFlavor::Unsupported => {
            panic!("Unsupported platform for thin linking")
        }
    }

    let extract_value = |arg: &str| -> Option<String> {
        original_args
            .iter()
            .position(|a| *a == arg)
            .map(|i| original_args[i + 1].to_string())
    };

    if let Some(vale) = extract_value("-target") {
        out_args.push("-target".to_string());
        out_args.push(vale);
    }

    if let Some(vale) = extract_value("-isysroot") {
        out_args.push("-isysroot".to_string());
        out_args.push(vale);
    }

    out_args
}

fn write_patch(
    ctx: &Context,
    exe: &Path,
    aslr_reference: u64,
    // artifacts: &mut BuildArtifacts,
    cache: &Arc<HotpatchModuleCache>,
    rustc_args: &RustcArgs,
    time_start: SystemTime,
) {
    let raw_args = std::fs::read_to_string(ctx.link_args_file.path()).unwrap();
    let args = raw_args.lines().collect::<Vec<_>>();

    // Extract out the incremental object files.
    //
    // This is sadly somewhat of a hack, but it might be a moderately reliable hack.
    //
    // When rustc links your project, it passes the args as how a linker would expect, but with
    // a somewhat reliable ordering. These are all internal details to cargo/rustc, so we can't
    // rely on them *too* much, but the *are* fundamental to how rust compiles your projects, and
    // linker interfaces probably won't change drastically for another 40 years.
    //
    // We need to tear apart this command and only pass the args that are relevant to our thin link.
    // Mainly, we don't want any rlibs to be linked. Occasionally some libraries like objc_exception
    // export a folder with their artifacts - unsure if we actually need to include them. Generally
    // you can err on the side that most *libraries* don't need to be linked here since dlopen
    // satisfies those symbols anyways when the binary is loaded.
    //
    // Many args are passed twice, too, which can be confusing, but generally don't have any real
    // effect. Note that on macos/ios, there's a special macho header that needs to be set, otherwise
    // dyld will complain.
    //
    // Also, some flags in darwin land might become deprecated, need to be super conservative:
    // - https://developer.apple.com/forums/thread/773907
    //
    // The format of this command roughly follows:
    // ```
    // clang
    //     /dioxus/target/debug/subsecond-cli
    //     /var/folders/zs/gvrfkj8x33d39cvw2p06yc700000gn/T/rustcAqQ4p2/symbols.o
    //     /dioxus/target/subsecond-dev/deps/subsecond_harness-acfb69cb29ffb8fa.05stnb4bovskp7a00wyyf7l9s.rcgu.o
    //     /dioxus/target/subsecond-dev/deps/subsecond_harness-acfb69cb29ffb8fa.08rgcutgrtj2mxoogjg3ufs0g.rcgu.o
    //     /dioxus/target/subsecond-dev/deps/subsecond_harness-acfb69cb29ffb8fa.0941bd8fa2bydcv9hfmgzzne9.rcgu.o
    //     /dioxus/target/subsecond-dev/deps/libbincode-c215feeb7886f81b.rlib
    //     /dioxus/target/subsecond-dev/deps/libanyhow-e69ac15c094daba6.rlib
    //     /dioxus/target/subsecond-dev/deps/libratatui-c3364579b86a1dfc.rlib
    //     /.rustup/toolchains/stable-aarch64-apple-darwin/lib/rustlib/aarch64-apple-darwin/lib/libstd-019f0f6ae6e6562b.rlib
    //     /.rustup/toolchains/stable-aarch64-apple-darwin/lib/rustlib/aarch64-apple-darwin/lib/libpanic_unwind-7387d38173a2eb37.rlib
    //     /.rustup/toolchains/stable-aarch64-apple-darwin/lib/rustlib/aarch64-apple-darwin/lib/libobject-2b03cf6ece171d21.rlib
    //     -framework AppKit
    //     -lc
    //     -framework Foundation
    //     -framework Carbon
    //     -lSystem
    //     -framework CoreFoundation
    //     -lobjc
    //     -liconv
    //     -lm
    //     -arch arm64
    //     -mmacosx-version-min=11.0.0
    //     -L /dioxus/target/subsecond-dev/build/objc_exception-dc226cad0480ea65/out
    //     -o /dioxus/target/subsecond-dev/deps/subsecond_harness-acfb69cb29ffb8fa
    //     -nodefaultlibs
    //     -Wl,-all_load
    // ```
    let mut dylibs = vec![];
    let mut object_files = args
        .iter()
        .filter(|arg| arg.ends_with(".rcgu.o"))
        .sorted()
        .map(PathBuf::from)
        .collect::<Vec<_>>();

    // On non-wasm platforms, we generate a special shim object file which converts symbols from
    // fat binary into direct addresses from the running process.
    //
    // Our wasm approach is quite specific to wasm. We don't need to resolve any missing symbols
    // there since wasm is relocatable, but there is considerable pre and post processing work to
    // satisfy undefined symbols that we do by munging the binary directly.
    //
    // todo: can we adjust our wasm approach to also use a similar system?
    // todo: don't require the aslr reference and just patch the got when loading.
    //
    // Requiring the ASLR offset here is necessary but unfortunately might be flakey in practice.
    // Android apps can take a long time to open, and a hot patch might've been issued in the interim,
    // making this hotpatch a failure.
    if !ctx.is_wasm_or_wasi() {
        let stub_bytes =
            create_undefined_symbol_stub(cache, &object_files, &ctx.triple, aslr_reference)
                .expect("failed to resolve patch symbols");

        // Currently we're dropping stub.o in the exe dir, but should probably just move to a tempfile?
        let patch_file = exe.with_file_name("stub.o");
        std::fs::write(&patch_file, stub_bytes).unwrap();
        object_files.push(patch_file);

        // Add the dylibs/sos to the linker args
        // Make sure to use the one in the bundle, not the ones in the target dir or system.
        for arg in &rustc_args.link_args {
            if arg.ends_with(".dylib") || arg.ends_with(".so") {
                let path = PathBuf::from(arg);
                dylibs.push(ctx.frameworks_directory().join(path.file_name().unwrap()));
            }
        }
    }

    // And now we can run the linker with our new args
    let linker = ctx.select_linker();
    let out_exe = patch_exe(ctx, time_start);
    let out_arg = match ctx.triple.operating_system {
        OperatingSystem::Windows => vec![format!("/OUT:{}", out_exe.display())],
        _ => vec!["-o".to_string(), out_exe.display().to_string()],
    };

    tracing::trace!("Linking with {:?} using args: {:#?}", linker, object_files);

    let mut out_args: Vec<OsString> = vec![];
    out_args.extend(object_files.iter().map(Into::into));
    out_args.extend(dylibs.iter().map(Into::into));
    out_args.extend(thin_link_args(ctx, &args).iter().map(Into::into));
    out_args.extend(out_arg.iter().map(Into::into));

    // TODO: windows
    // if cfg!(windows) {
    //     let cmd_contents: String = out_args
    //         .iter()
    //         .map(|s| format!("\"{}\"", s.to_string_lossy()))
    //         .join(" ");
    //     std::fs::write(self.command_file.path(), cmd_contents).unwrap();
    //     out_args = vec![format!("@{}", self.command_file.path().display()).into()];
    // }

    // Run the linker directly!
    //
    // We dump its output directly into the patch exe location which is different than how rustc
    // does it since it uses llvm-objcopy into the `target/debug/` folder.
    let mut linker_command = Command::new(linker);
    linker_command
        .args(out_args)
        .env_clear()
        .envs(rustc_args.envs.iter().map(|(k, v)| (k, v)));
    let res = linker_command.output().unwrap();

    if !res.stderr.is_empty() {
        let errs = String::from_utf8_lossy(&res.stderr);
        if !patch_exe(ctx, time_start).exists() || !res.status.success() {
            tracing::error!("Failed to generate patch: {}", errs.trim());
        } else {
            tracing::trace!("Linker output during thin linking: {}", errs.trim());
        }
    }

    // For some really weird reason that I think is because of dlopen caching, future loads of the
    // jump library will fail if we don't remove the original fat file. I think this could be
    // because of library versioning and namespaces, but really unsure.
    //
    // The errors if you forget to do this are *extremely* cryptic - missing symbols that never existed.
    //
    // Fortunately, this binary exists in two places - the deps dir and the target out dir. We
    // can just remove the one in the deps dir and the problem goes away.
    if let Some(idx) = args.iter().position(|arg| *arg == "-o") {
        _ = std::fs::remove_file(PathBuf::from(args[idx + 1]));
    }

    // Now extract the assets from the fat binary
    // artifacts.assets = self.collect_assets(&self.patch_exe(artifacts.time_start), ctx)?;

    // If this is a web build, reset the index.html file in case it was modified by SSG
    // self.write_index_html(&artifacts.assets)
    //     .context("Failed to write index.html")?;

    // Clean up the temps manually
    // todo: we might want to keep them around for debugging purposes
    for file in object_files {
        _ = std::fs::remove_file(file);
    }
}

fn build_thin(
    ctx: &Context,
    rustc_args: &RustcArgs,
    aslr_reference: u64,
    cache: &Arc<HotpatchModuleCache>,
) -> SystemTime {
    let time_start = SystemTime::now();
    let mut cmd = build_thin_command(ctx, rustc_args);
    let mut process = cmd.spawn().unwrap();
    process.wait().unwrap();
    let compiled_exe = ctx
        .target_dir
        .join(ctx.triple.to_string())
        .join(&ctx.profile_dir)
        .join(&ctx.final_binary_name());

    write_patch(
        ctx,
        &compiled_exe,
        aslr_reference,
        cache,
        rustc_args,
        time_start,
    );

    // let bundle_exe = ctx.bundle_path.join(&ctx.bin);
    // std::fs::copy(&compiled_exe, &bundle_exe).unwrap();

    time_start
}

fn build_thin_command(ctx: &Context, rustc_args: &RustcArgs) -> Command {
    let mut cmd = Command::new("rustc");
    cmd.current_dir(&ctx.working_dir)
        .env_clear()
        .args(rustc_args.args[1..].iter())
        .env_remove("RUSTC_WORKSPACE_WRAPPER")
        .env_remove("RUSTC_WRAPPER")
        .env_remove("DX_RUSTC")
        .env("DX_LINK", "1")
        .env(
            "DX_LINK_ARGS_FILE",
            ctx.link_args_file.path().to_owned().canonicalize().unwrap(),
        )
        .env(
            "DX_LINK_ERR_FILE",
            ctx.link_err_file.path().canonicalize().unwrap(),
        )
        .env("DX_LINK_TRIPLE", ctx.triple.to_string())
        .arg(format!("-Clinker=dx"));

    if ctx.is_wasm_or_wasi() {
        cmd.arg("-Crelocation-model=pic");
    }

    cmd.envs(rustc_args.envs.iter().cloned());

    cmd
}

fn clean_fingerprint(ctx: &Context) {
    // `dx` compiles everything with `--target` which ends up with a structure like:
    // target/<triple>/<profile>/.fingerprint/<package_name>-<hash>
    //
    // normally you can't rely on this structure (ie with `cargo build`) but the explicit
    // target arg guarantees this will work.
    let fingerprint_dir = ctx
        .target_dir
        .join(ctx.triple.to_string())
        .join(&ctx.profile_dir)
        .join(".fingerprint");

    // split at the last `-` used to separate the hash from the name
    // This causes to more aggressively bust hashes for all combinations of features
    // and fingerprints for this package since we're just ignoring the hash

    if let Ok(dirs) = std::fs::read_dir(&fingerprint_dir) {
        for entry in dirs.flatten() {
            if let Some(fname) = entry.file_name().to_str() {
                if let Some((name, _)) = fname.rsplit_once('-') {
                    if name == ctx.package {
                        _ = std::fs::remove_dir_all(entry.path());
                    }
                }
            }
        }
    }
}

fn fat_link(ctx: &Context, exe: &Path, rustc_args: &RustcArgs) {
    // Filter out the rlib files from the arguments
    let rlibs = rustc_args
        .link_args
        .iter()
        .filter(|arg| arg.ends_with(".rlib"))
        .map(PathBuf::from)
        .collect::<Vec<_>>();

    // Acquire a hash from the rlib names, sizes, modified times, and dx's git commit hash
    // This ensures that any changes in dx or the rlibs will cause a new hash to be generated
    // The hash relies on both dx and rustc hashes, so it should be thoroughly unique. Keep it
    // short to avoid long file names.
    let hash_id = Uuid::new_v5(
        &Uuid::NAMESPACE_OID,
        rlibs
            .iter()
            .map(|p| {
                format!(
                    "{}-{}-{}",
                    p.file_name().unwrap().to_string_lossy(),
                    p.metadata().map(|m| m.len()).unwrap_or_default(),
                    p.metadata()
                        .ok()
                        .and_then(|m| m.modified().ok())
                        .and_then(|f| f
                            .duration_since(std::time::UNIX_EPOCH)
                            .map(|f| f.as_secs())
                            .ok())
                        .unwrap_or_default(),
                )
            })
            .collect::<String>()
            .as_bytes(),
    )
    .to_string()
    .chars()
    .take(8)
    .collect::<String>();

    // Check if we already have a cached object file
    let out_ar_path = exe.with_file_name(format!("libdeps-{hash_id}.a",));
    let out_rlibs_list = exe.with_file_name(format!("rlibs-{hash_id}.txt"));
    let mut archive_has_contents = out_ar_path.exists();

    // Use the rlibs list if it exists
    let mut compiler_rlibs = std::fs::read_to_string(&out_rlibs_list)
        .ok()
        .map(|s| s.lines().map(PathBuf::from).collect::<Vec<_>>())
        .unwrap_or_default();

    // Create it by dumping all the rlibs into it
    // This will include the std rlibs too, which can severely bloat the size of the archive
    //
    // The nature of this process involves making extremely fat archives, so we should try and
    // speed up the future linking process by caching the archive.
    //
    // Since we're using the git hash for the CLI entropy, debug builds should always regenerate
    // the archive since their hash might not change, but the logic might.
    if !archive_has_contents || cfg!(debug_assertions) {
        compiler_rlibs.clear();

        let mut bytes = vec![];
        let mut out_ar = ar::Builder::new(&mut bytes);
        for rlib in &rlibs {
            // Skip compiler rlibs since they're missing bitcode
            //
            // https://github.com/rust-lang/rust/issues/94232#issuecomment-1048342201
            //
            // if the rlib is not in the target directory, we skip it.
            if !rlib.starts_with(&ctx.working_dir) {
                compiler_rlibs.push(rlib.clone());
                tracing::trace!("Skipping rlib: {:?}", rlib);
                continue;
            }

            tracing::trace!("Adding rlib to staticlib: {:?}", rlib);

            let rlib_contents = std::fs::read(rlib).unwrap();
            let mut reader = ar::Archive::new(std::io::Cursor::new(rlib_contents));
            let mut keep_linker_rlib = false;
            while let Some(Ok(object_file)) = reader.next_entry() {
                let name = std::str::from_utf8(object_file.header().identifier()).unwrap();
                if name.ends_with(".rmeta") {
                    continue;
                }

                if object_file.header().size() == 0 {
                    continue;
                }

                // rlibs might contain dlls/sos/lib files which we don't want to include
                //
                // This catches .dylib, .so, .dll, .lib, .o, etc files that are not compatible with
                // our "fat archive" linking process.
                //
                // We only trust `.rcgu.o` files to make it into the --all_load archive.
                // This is a temporary stopgap to prevent issues with libraries that generate
                // object files that are not compatible with --all_load.
                // see https://github.com/DioxusLabs/dioxus/issues/4237
                if !(name.ends_with(".rcgu.o") || name.ends_with(".obj")) {
                    keep_linker_rlib = true;
                    continue;
                }

                archive_has_contents = true;
                out_ar
                    .append(&object_file.header().clone(), object_file)
                    .unwrap();
            }

            // Some rlibs contain weird artifacts that we don't want to include in the fat archive.
            // However, we still want them around in the linker in case the regular linker can handle them.
            if keep_linker_rlib {
                compiler_rlibs.push(rlib.clone());
            }
        }

        let bytes = out_ar.into_inner().unwrap();
        std::fs::write(&out_ar_path, bytes).unwrap();
        tracing::debug!("Wrote fat archive to {:?}", out_ar_path);

        // Run the ranlib command to index the archive. This slows down this process a bit,
        // but is necessary for some linkers to work properly.
        // We ignore its error in case it doesn't recognize the architecture
        // if ctx.linker_flavor == LinkerFlavor::Darwin {
        //     if let Some(ranlib) = Workspace::select_ranlib() {
        //         _ = Command::new(ranlib).arg(&out_ar_path).output();
        //     }
        // }
    }

    compiler_rlibs.dedup();

    // We're going to replace the first rlib in the args with our fat archive
    // And then remove the rest of the rlibs
    //
    // We also need to insert the -force_load flag to force the linker to load the archive
    let mut args: Vec<_> = rustc_args.link_args.iter().skip(1).cloned().collect();
    if let Some(last_object) = args.iter().rposition(|arg| arg.ends_with(".o")) {
        if archive_has_contents {
            match ctx.linker_flavor {
                LinkerFlavor::WasmLld => {
                    args.insert(last_object, "--whole-archive".to_string());
                    args.insert(last_object + 1, out_ar_path.display().to_string());
                    args.insert(last_object + 2, "--no-whole-archive".to_string());
                    args.retain(|arg| !arg.ends_with(".rlib"));
                    for rlib in compiler_rlibs.iter().rev() {
                        args.insert(last_object + 3, rlib.display().to_string());
                    }
                }
                LinkerFlavor::Gnu => {
                    args.insert(last_object, "-Wl,--whole-archive".to_string());
                    args.insert(last_object + 1, out_ar_path.display().to_string());
                    args.insert(last_object + 2, "-Wl,--no-whole-archive".to_string());
                    args.retain(|arg| !arg.ends_with(".rlib"));
                    for rlib in compiler_rlibs.iter().rev() {
                        args.insert(last_object + 3, rlib.display().to_string());
                    }
                }
                LinkerFlavor::Darwin => {
                    args.insert(last_object, "-Wl,-force_load".to_string());
                    args.insert(last_object + 1, out_ar_path.display().to_string());
                    args.retain(|arg| !arg.ends_with(".rlib"));
                    for rlib in compiler_rlibs.iter().rev() {
                        args.insert(last_object + 2, rlib.display().to_string());
                    }
                }
                LinkerFlavor::Msvc => {
                    args.insert(
                        last_object,
                        format!("/WHOLEARCHIVE:{}", out_ar_path.display()),
                    );
                    args.retain(|arg| !arg.ends_with(".rlib"));
                    for rlib in compiler_rlibs.iter().rev() {
                        args.insert(last_object + 1, rlib.display().to_string());
                    }
                }
                LinkerFlavor::Unsupported => {
                    tracing::error!("Unsupported platform for fat linking");
                }
            };
        }
    }

    // Add custom args to the linkers
    match ctx.linker_flavor {
        LinkerFlavor::Gnu => {
            // Export `main` so subsecond can use it for a reference point
            args.push("-Wl,--export-dynamic-symbol,main".to_string());
        }
        LinkerFlavor::Darwin => {
            args.push("-Wl,-exported_symbol,_main".to_string());
        }
        LinkerFlavor::Msvc => {
            // Prevent alsr from overflowing 32 bits
            args.push("/HIGHENTROPYVA:NO".to_string());

            // Export `main` so subsecond can use it for a reference point
            args.push("/EXPORT:main".to_string());
        }
        LinkerFlavor::WasmLld | LinkerFlavor::Unsupported => {}
    }

    // We also need to remove the `-o` flag since we want the linker output to end up in the
    // rust exe location, not in the deps dir as it normally would.
    if let Some(idx) = args
        .iter()
        .position(|arg| *arg == "-o" || *arg == "--output")
    {
        args.remove(idx + 1);
        args.remove(idx);
    }

    // same but windows support
    if let Some(idx) = args.iter().position(|arg| arg.starts_with("/OUT")) {
        args.remove(idx);
    }

    // We want to go through wasm-ld directly, so we need to remove the -flavor flag
    if ctx.is_wasm_or_wasi() {
        let flavor_idx = args.iter().position(|arg| *arg == "-flavor").unwrap();
        args.remove(flavor_idx + 1);
        args.remove(flavor_idx);
    }

    // Set the output file
    match ctx.triple.operating_system {
        OperatingSystem::Windows => args.push(format!("/OUT:{}", exe.display())),
        _ => args.extend(["-o".to_string(), exe.display().to_string()]),
    }

    // And now we can run the linker with our new args
    let linker = ctx.select_linker();

    tracing::trace!("Fat linking with args: {:?} {:#?}", linker, args);
    tracing::trace!("Fat linking with env:");
    for e in rustc_args.envs.iter() {
        tracing::trace!("  {}={}", e.0, e.1);
    }

    // Handle windows command files
    let out_args = args.clone();
    // if cfg!(windows) {
    //     let cmd_contents: String = out_args.iter().map(|f| format!("\"{f}\"")).join(" ");
    //     std::fs::write(self.command_file.path(), cmd_contents).unwrap();
    //     out_args = vec![format!("@{}", self.command_file.path().display())];
    // }

    // Run the linker directly!
    let res = Command::new(linker)
        .args(out_args)
        .env_clear()
        .envs(rustc_args.envs.iter().map(|(k, v)| (k, v)))
        .output()
        .unwrap();

    if !res.stderr.is_empty() {
        let errs = String::from_utf8_lossy(&res.stderr);
        if !res.status.success() {
            tracing::error!("Failed to generate fat binary: {}", errs.trim());
        } else {
            tracing::trace!("Warnings during fat linking: {}", errs.trim());
        }
    }

    if !res.stdout.is_empty() {
        let out = String::from_utf8_lossy(&res.stdout);
        tracing::trace!("Output from fat linking: {}", out.trim());
    }

    // Clean up the temps manually
    for f in args.iter().filter(|arg| arg.ends_with(".rcgu.o")) {
        _ = std::fs::remove_file(f);
    }

    // Cache the rlibs list
    _ = std::fs::write(
        &out_rlibs_list,
        compiler_rlibs
            .into_iter()
            .map(|s| s.display().to_string())
            .join("\n"),
    );
}

fn build_fat(ctx: &Context) -> PathBuf {
    clean_fingerprint(ctx);

    let mut cmd = build_fat_command(ctx);
    let mut process = cmd.spawn().unwrap();
    process.wait().unwrap();

    // let mut perms = std::fs::metadata(&bundle_exe).unwrap().permissions();
    // perms.set_mode(0o700);
    // std::fs::set_permissions(&bundle_exe, perms).unwrap();

    let compiled_exe = ctx
        .target_dir
        .join(ctx.triple.to_string())
        .join(&ctx.profile_dir)
        .join(&ctx.final_binary_name());

    let mut rustc_args: RustcArgs =
        serde_json::from_str(&std::fs::read_to_string(ctx.rustc_wrapper_file.path()).unwrap())
            .unwrap();
    rustc_args.link_args = std::fs::read_to_string(ctx.link_args_file.path())
        .unwrap()
        .lines()
        .map(|s| s.to_string())
        .collect::<Vec<_>>();

    fat_link(ctx, &compiled_exe, &rustc_args);

    let bundle_exe = write_executable(ctx, &compiled_exe);

    bundle_exe
}

fn build_fat_command(ctx: &Context) -> Command {
    let mut command = Command::new("cargo");
    command
        .env(
            "DX_RUSTC",
            ctx.rustc_wrapper_file
                .path()
                .to_owned()
                .canonicalize()
                .unwrap(),
        )
        // .arg("--verbose")
        .env("RUSTC_WRAPPER", "dx")
        .env("DX_LINK", "1")
        .env(
            "DX_LINK_ARGS_FILE",
            ctx.link_args_file.path().to_owned().canonicalize().unwrap(),
        )
        .env(
            "DX_LINK_ERR_FILE",
            ctx.link_err_file.path().canonicalize().unwrap(),
        )
        .env("DX_LINK_TRIPLE", ctx.triple.to_string())
        .arg("rustc")
        .current_dir(&ctx.working_dir)
        .arg("--profile")
        .arg(&ctx.profile_name)
        .arg("-p")
        .arg(&ctx.package)
        .arg("--target")
        .arg(ctx.triple.to_string());
    if ctx.no_default_features {
        command.arg("--no-default-features");
    }
    if let Some(bin_name) = &ctx.bin {
        command.arg("--bin").arg(bin_name);
    }
    if ctx.lib {
        command.arg("--lib");
    }
    if !ctx.features.is_empty() {
        command.arg("--features");
        for feature in &ctx.features {
            command.arg(feature);
        }
    }
    // .arg("--message-format")
    // .arg("json-diagnostic-rendered-ansi")
    command.arg("--").arg("-Clinker=dx");
    if ctx.triple.operating_system == OperatingSystem::Linux {
        command
            .arg("-Clink-arg=-Wl,-rpath,$ORIGIN/../lib")
            .arg("-Clink-arg=-Wl,-rpath,$ORIGIN");
    }
    command.arg("-Csave-temps=true").arg("-Clink-dead-code");

    let mut rust_flags = ctx
        .rust_flags
        .iter()
        .map(|flag| {
            if flag.starts_with("cfg") {
                "--".to_string() + flag
            } else {
                flag.clone()
            }
        })
        .collect::<Vec<_>>();

    if ctx.is_wasm_or_wasi() {
        rust_flags.push("-Ctarget-cpu=mvp".to_string());
    }

    if !ctx.rust_flags.is_empty() {
        command.env("RUSTFLAGS", rust_flags.join(" "));
    }

    if ctx.is_wasm_or_wasi() {
        command
            .arg("-Ctarget-cpu=mvp")
            .arg("-Clink-arg=--no-gc-sections")
            .arg("-Clink-arg=--growable-table")
            .arg("-Clink-arg=--export-table")
            .arg("-Clink-arg=--export-memory")
            .arg("-Clink-arg=--emit-relocs")
            .arg("-Clink-arg=--export=__stack_pointer")
            .arg("-Clink-arg=--export=__heap_base")
            .arg("-Clink-arg=--export=__data_end");
    }

    command
}
