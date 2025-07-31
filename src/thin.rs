use std::ffi::OsString;
use std::path::Path;
use std::sync::Arc;
use std::time::SystemTime;
use std::{path::PathBuf, process::Command};

use crate::context::Context;
use crate::patch::{HotpatchModuleCache, create_undefined_symbol_stub};
use crate::{LinkerFlavor, RustcArgs, patch_exe};
use itertools::Itertools;
use target_lexicon::OperatingSystem;

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

pub fn build_thin(
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
