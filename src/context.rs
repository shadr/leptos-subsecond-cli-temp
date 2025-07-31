use std::path::PathBuf;

use target_lexicon::Triple;
use tempfile::NamedTempFile;

use crate::LinkerFlavor;

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
}
