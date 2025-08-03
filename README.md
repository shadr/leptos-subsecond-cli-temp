# How to run

```sh
git clone https://github.com/shadr/leptos-subsecond-cli-temp
cd leptos-subsecond-cli-temp
RUST_LOG=debug LEPTOS_OUTPUT_NAME=your-package cargo run --release -- --manifest-path ../your-project/Cargo.toml leptos --package your-package --server-bin your-server-bin --server-features ssr --lib-features hydrate --server-no-default-features --lib-no-default-features
```

Currently there is no file watcher, so to hot reload or do a full rebuild you need to enter "r" or "R" symbols into stdin respectively.

# How it works

Here I outline how my understanding of how fat and thin builds work:

## Fat builds
Fat builds are big and slow, we compile every dependency into a shared library without removing any dead code

1) Clear fingerprints for the top-level crate to trigger rebuild (this will be needed in step 3)
2) Run `cargo rustc` with needed flags and env variables
   - `-Clinker=dx` we pass `dx` as a linker wrapper to store linker arguments to a file
   - `-Clink-dead-code` do not omit dead code when linking
   - `RUSTC_WRAPPER` `dx` we pass `dx` also as a rustc wrapper to store its arguments
   - `DX_RUSTC` path to a file where to store rustc arguments
   - `DX_LINK_ARGS_FILE` path to a file where to store linker arguments
   - a few link arguments for handling wasm builds
3) Link fat build, take into account every `.o` file in every rlib
4) Read rustc arguments from a `DX_RUSTC` file
5) Create a HotpatchModuleCache with compiled binary
6) (Optional) run the binary, binary should tell the build tool its aslr reference (e.g. passing it when connecting to websocket)

## Thin builds
Thin builds are uses `rustc` directly

1) Build crate object file with rustc
   - `-Clinker=dx` used to pass/filter custom linker arguments
   - `DX_LINK_ARGS_FILE` file path to read saved linker arguments from
2) Link using custom linker setup and previously saved aslr reference and rustc arguments to generate a patch library file
3) Create a jump table using HotpatchModuleCache
4) Send the jump table to the connected clients

# Original code references
Here is links to build related functions in original dioxus-cli code:

- [BuildRequest::build](https://github.com/DioxusLabs/dioxus/blob/d8d11db403f1d32f4ca89f413f40899c5a279fc5/packages/cli/src/build/request.rs#L832) - do either fat or thin build including linking
- [BuildRequest::cargo_build](https://github.com/DioxusLabs/dioxus/blob/d8d11db403f1d32f4ca89f413f40899c5a279fc5/packages/cli/src/build/request.rs#L893) - create a `cargo rustc` or `rustc` command and run it with setted env variables and arguments, if it is a fat build also link the binary
- [BuildRequest::run_fat_link](https://github.com/DioxusLabs/dioxus/blob/d8d11db403f1d32f4ca89f413f40899c5a279fc5/packages/cli/src/build/request.rs#L1746) - link fat binary
- [BuildRequest::write_patch](https://github.com/DioxusLabs/dioxus/blob/d8d11db403f1d32f4ca89f413f40899c5a279fc5/packages/cli/src/build/request.rs#L1339) - custom linker setup for thin builds to generate a patch file
- [AppBuilder::hotpatch](https://github.com/DioxusLabs/dioxus/blob/d8d11db403f1d32f4ca89f413f40899c5a279fc5/packages/cli/src/build/builder.rs#L635) - using compiled thin binary and HotpatchModuleCache creates a jump table and sends it to the running binaries
- [create_jump_table](https://github.com/DioxusLabs/dioxus/blob/d8d11db403f1d32f4ca89f413f40899c5a279fc5/packages/cli/src/build/patch.rs#L294) - creates a jump table, this whole file can be copied without any modifications, it doesn't have any dependencies on cli code
