* How to run
```sh
git clone https://github.com/shadr/leptos-subsecond-cli-temp
RUST_LOG=debug LEPTOS_OUTPUT_NAME=your-package cargo run --release -- --manifest-path ../your-project/Cargo.toml leptos --package your-package --server-bin your-server-bin --server-features ssr --lib-features hydrate --server-no-default-features --lib-no-default-features
```
