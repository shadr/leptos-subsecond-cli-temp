use std::{
    path::{Path, PathBuf},
    process::{Child, Command},
    sync::{Arc, atomic::AtomicU64},
    time::Instant,
};

use dioxus_devtools::DevserverMsg;
use multiqueue::BroadcastSender;

use crate::{
    RustcArgs,
    context::Context,
    patch::{HotpatchModuleCache, create_jump_table},
    thin,
};

pub struct Builder {
    pub ctx: Context,
    cache: Arc<HotpatchModuleCache>,
    rustc_args: RustcArgs,
    patch_sender: BroadcastSender<DevserverMsg>,
    pid: Option<u32>,
    aslr_reference: Arc<AtomicU64>,
    running_binary: Option<Child>,
}

impl Builder {
    pub fn new(
        ctx: Context,
        patch_sender: BroadcastSender<DevserverMsg>,
        aslr_reference: Arc<AtomicU64>,
    ) -> Self {
        Self {
            ctx,
            cache: Arc::new(HotpatchModuleCache::default()),
            rustc_args: RustcArgs::default(),
            patch_sender,
            pid: None,
            aslr_reference,
            running_binary: None,
        }
    }

    pub fn build_fat(&mut self) -> PathBuf {
        let path = crate::fat::build_fat(&self.ctx);

        self.rustc_args = serde_json::from_str(
            &std::fs::read_to_string(self.ctx.rustc_wrapper_file.path()).unwrap(),
        )
        .unwrap();

        self.cache = Arc::new(HotpatchModuleCache::new(&path, &self.ctx.triple).unwrap());

        path
    }

    pub fn build_thin(&self) {
        let aslr_reference = self
            .aslr_reference
            .load(std::sync::atomic::Ordering::SeqCst);
        if !self.ctx.is_wasm_or_wasi() && aslr_reference == 0 {
            tracing::error!("Thin build canceled, aslr reference is 0 on non-wasm build!");
            return;
        }
        let time_start = thin::build_thin(&self.ctx, &self.rustc_args, aslr_reference, &self.cache);

        let new = self.ctx.patch_exe(time_start);
        let now = Instant::now();
        let mut jump_table = create_jump_table(&new, &self.ctx.triple, &self.cache).unwrap();
        tracing::debug!("Created jump table in {}s", now.elapsed().as_secs_f32());

        if self.ctx.triple.architecture == target_lexicon::Architecture::Wasm32 {
            // Make sure we use the dir relative to the public dir, so the web can load it as a proper URL
            //
            // ie we would've shipped `/Users/foo/Projects/dioxus/target/dx/project/debug/web/public/wasm/lib.wasm`
            //    but we want to ship `/wasm/lib.wasm`
            let patch_lib_name = jump_table.lib.file_name().unwrap();
            self.ctx.write_thin_wasm_patch_to_pkg(&jump_table.lib);
            jump_table.lib = PathBuf::from("/pkg/").join(patch_lib_name);
        }

        let msg = DevserverMsg::HotReload(dioxus_devtools::HotReloadMsg {
            templates: Vec::new(),
            assets: Vec::new(),
            ms_elapsed: 0,
            jump_table: Some(jump_table),
            for_build_id: None,
            for_pid: self.pid,
        });
        self.patch_sender.try_send(msg).unwrap();
        tracing::info!(
            "Hot-patch created in {}s",
            time_start.elapsed().unwrap().as_secs_f32()
        );
    }

    pub fn run_if_native(&mut self, path: &Path) {
        if self.ctx.bin.is_some() {
            let mut exe_cmd = Command::new(path);
            exe_cmd.env("LEPTOS_OUTPUT_NAME", &self.ctx.package);
            let new_exe = exe_cmd.spawn().unwrap();
            self.pid = Some(new_exe.id());
            self.running_binary = Some(new_exe);
        }
    }
}

impl Drop for Builder {
    fn drop(&mut self) {
        if let Some(process) = &mut self.running_binary {
            match process.kill() {
                Ok(_) => tracing::debug!("Killed executable successfully"),
                Err(_) => tracing::error!("Couldn't kill running executable!"),
            }
        }
    }
}
