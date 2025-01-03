Installation: 
```bash
cargo build --package cargo-android --release
cp target/release/cargo-android ~/.cargo/bin/
```

Usage:
```bash
cargo-android apk build --package <<package>> [--release]
cargo-android aab build [--release]
```