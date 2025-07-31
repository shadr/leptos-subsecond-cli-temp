use std::{
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use target_lexicon::Triple;
use tempfile::NamedTempFile;
use wasm_bindgen_cli_support::Bindgen;

use crate::{LinkerFlavor, patch::prepare_wasm_base_module};

pub struct Context {
    pub working_dir: PathBuf,
    pub target_dir: PathBuf,
    pub bin: Option<String>,
    pub lib: bool,
    pub triple: Triple,
    pub rustc_wrapper_file: NamedTempFile,
    pub link_args_file: NamedTempFile,
    pub link_err_file: NamedTempFile,
    pub bundle_path: PathBuf,
    pub profile_dir: String,
    pub profile_name: String,
    pub package: String,
    pub linker_flavor: LinkerFlavor,
    pub features: Vec<String>,
    pub rust_flags: Vec<String>,
    pub no_default_features: bool,
}

impl Context {
    pub fn frameworks_directory(&self) -> PathBuf {
        self.target_dir.join("frameworks")
    }

    pub fn select_linker(&self) -> PathBuf {
        match self.linker_flavor {
            LinkerFlavor::WasmLld => PathBuf::from("wasm-ld"),
            LinkerFlavor::Gnu => PathBuf::from("cc"),
            _ => PathBuf::from("cc"),
        }
    }

    pub fn is_wasm_or_wasi(&self) -> bool {
        matches!(
            self.triple.architecture,
            target_lexicon::Architecture::Wasm32 | target_lexicon::Architecture::Wasm64
        ) || self.triple.operating_system == target_lexicon::OperatingSystem::Wasi
    }

    pub fn final_binary_name(&self) -> String {
        if let Some(bin_name) = &self.bin {
            return bin_name.clone();
        }
        if self.lib {
            return self.package.clone();
        }
        unreachable!("you should specify either bin {{name}} or lib");
    }

    pub fn patch_exe(&self, time_start: SystemTime) -> PathBuf {
        let compiled_exe = self
            .target_dir
            .join(self.triple.to_string())
            .join(&self.profile_dir)
            .join(self.final_binary_name());
        let path = compiled_exe.with_file_name(format!(
            "lib{}-patch-{}",
            self.final_binary_name(),
            time_start
                .duration_since(UNIX_EPOCH)
                .map(|f| f.as_millis())
                .unwrap_or(0),
        ));

        let extension = match self.linker_flavor {
            LinkerFlavor::Darwin => "dylib",
            LinkerFlavor::Gnu => "so",
            LinkerFlavor::WasmLld => "wasm",
            LinkerFlavor::Msvc => "dll",
            LinkerFlavor::Unsupported => "",
        };

        path.with_extension(extension)
    }

    pub fn write_executable(&self, compiled: &Path) -> PathBuf {
        if self.is_wasm_or_wasi() {
            self.write_wasm(compiled)
        } else {
            self.write_native(compiled)
        }
    }

    pub fn write_native(&self, binary: &Path) -> PathBuf {
        let bundle_exe = self.bundle_path.join(&self.final_binary_name());
        std::fs::copy(&binary, &bundle_exe).unwrap();
        bundle_exe
    }

    pub fn write_wasm(&self, wasm: &Path) -> PathBuf {
        let wasm_bindgen_dir = self.target_dir.join("wasm-bindgen/");
        let _ = std::fs::remove_dir_all(&wasm_bindgen_dir);
        std::fs::create_dir_all(&wasm_bindgen_dir).unwrap();

        tracing::info!("preparing wasm file for bindgen");
        let unprocessed = std::fs::read(wasm).unwrap();
        let all_exported_bytes = prepare_wasm_base_module(&unprocessed).unwrap();
        std::fs::write(wasm, all_exported_bytes).unwrap();
        tracing::info!("preparing wasm file done");

        tracing::info!("running wasm-bindgen");
        let mut bindgen = Bindgen::new()
            .keep_lld_exports(true)
            .demangle(false) // do no demangle names, hotpatchmodulecache ifunc map not populated properly with demangled names for some reason
            .debug(true)
            .keep_debug(true)
            .input_path(wasm)
            .out_name(&self.final_binary_name())
            .web(true)
            .unwrap()
            .generate_output()
            .unwrap();

        bindgen.emit(&wasm_bindgen_dir).unwrap();
        tracing::info!("wasm-bindgen done");

        let new_wasm_path = wasm_bindgen_dir
            .join(self.final_binary_name())
            .with_extension("wasm");

        std::fs::rename(
            wasm_bindgen_dir.join(format!("{}_bg.wasm", self.final_binary_name())),
            &new_wasm_path,
        )
        .unwrap();

        std::fs::create_dir_all(self.target_dir.join("site/pkg")).unwrap();

        std::fs::copy(
            &new_wasm_path,
            self.target_dir
                .join("site/pkg")
                .join(new_wasm_path.file_name().unwrap()),
        )
        .unwrap();

        let js_path = new_wasm_path.with_file_name(format!("{}.js", self.final_binary_name()));

        std::fs::copy(
            &js_path,
            self.target_dir
                .join("site/pkg")
                .join(js_path.file_name().unwrap()),
        )
        .unwrap();

        new_wasm_path
    }
}
