use std::path::Path;
use std::time::Instant;
use std::{path::PathBuf, process::Command};

use itertools::Itertools;
use target_lexicon::OperatingSystem;
use uuid::Uuid;

use crate::context::Context;
use crate::{LinkerFlavor, RustcArgs};

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
        // TODO: darwin
        // if ctx.linker_flavor() == LinkerFlavor::Darwin {
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
            match ctx.linker_flavor() {
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
    match ctx.linker_flavor() {
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
    // TODO: windows
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

fn clean_fingerprint(ctx: &Context) {
    // `dx` compiles everything with `--target` which ends up with a structure like:
    // target/<triple>/<profile>/.fingerprint/<package_name>-<hash>
    //
    // normally you can't rely on this structure (ie with `cargo build`) but the explicit
    // target arg guarantees this will work.
    let fingerprint_dir = ctx.target_triple_profile_dir().join(".fingerprint");

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

pub fn build_fat(ctx: &Context) -> PathBuf {
    tracing::debug!("Fat build started");
    let time_start = Instant::now();

    clean_fingerprint(ctx);

    let mut cmd = build_fat_command(ctx);
    let mut process = cmd.spawn().unwrap();
    process.wait().unwrap();

    let compiled_exe = ctx
        .target_triple_profile_dir()
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

    let bundle_exe = ctx.write_executable(&compiled_exe);
    // TODO: write frameworks

    tracing::debug!(
        "Fat build finished in {}s",
        time_start.elapsed().as_secs_f32()
    );

    bundle_exe
}

pub fn build_fat_command(ctx: &Context) -> Command {
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
