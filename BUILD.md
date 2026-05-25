# Building GitTerm

## First-Time Setup (Any Platform)

After cloning, activate the local git hooks once so commits run the same
gates CI does (`cargo clippy -- -D warnings && cargo fmt --check && cargo test`)
and reject commit messages without a `TRU-NN` Linear key:

```bash
git config core.hooksPath .githooks
```

This is a per-clone setting; you don't need to commit it.

## macOS (Local Build)

### Development Build
```bash
cargo build
cargo run
```

### Release Build & App Bundle
```bash
./scripts/bundle.sh
```

This creates `target/GitTerm.app` which you can:
- Copy to Applications: `cp -r target/GitTerm.app /Applications/`
- Or open directly: `open target/GitTerm.app`

## Linux (Local Build)

### Prerequisites

Install system dependencies (Ubuntu/Debian):

```bash
sudo apt-get update
sudo apt-get install -y \
  build-essential pkg-config cmake \
  libxcb-shape0-dev libxcb-xfixes0-dev libxkbcommon-dev \
  libssl-dev libasound2-dev libclang-dev \
  libglib2.0-dev libgtk-3-dev libpango1.0-dev \
  libatk1.0-dev libgdk-pixbuf2.0-dev \
  libsoup-3.0-dev libwebkit2gtk-4.1-dev libxdo-dev
```

For Fedora/RHEL:

```bash
sudo dnf install -y \
  gcc pkg-config cmake openssl-devel \
  libxcb-devel libxkbcommon-devel alsa-lib-devel clang-devel \
  gtk3-devel pango-devel atk-devel gdk-pixbuf2-devel
```

Install Rust if you haven't already:

```bash
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
```

### Clone & Build

GitTerm depends on a custom `iced_term` fork that needs to be cloned alongside it:

```bash
git clone https://github.com/Tru-Insights/gitterm.git
git clone https://github.com/Tru-Insights/iced_term.git iced_term_fork
cd gitterm
cargo build --release --features stt
```

Binary at: `target/release/gitterm`

Feature flags:
- `stt` — voice (whisper-rs + cpal). Required for shipping.
- `excalidraw` — diagram viewer surface. Optional.
- `./scripts/bundle.sh` builds with both (`stt excalidraw`).

### Running

```bash
./target/release/gitterm
```

## Cross-Platform Builds (GitHub Actions)

GitHub Actions builds run on:
- Push to `master`
- Tag push (e.g. `v1.0.0`) — also creates a Release with attached binaries
- Manual workflow dispatch

**Platforms built:** macOS (x86_64 + aarch64) `.app`, Windows `.exe`, Linux binary.

### Before triggering CI

All three workflows (`build.yml`, `ci.yml`, `windows-build.yml`) clone
`https://github.com/Tru-Insights/iced_term.git` master fresh on every run.
If you've made local commits to `../iced_term_fork` that gitterm now depends
on, **push them first** — otherwise CI compiles against the old API and fails
with errors like `no method named X found`.

```bash
cd ../iced_term_fork
git log @{u}..HEAD --oneline   # must be empty before CI
git push origin master          # if it's not
```

### Creating a Release

1. Tag your commit:
   ```bash
   git tag v1.0.0
   git push origin v1.0.0
   ```

2. GitHub Actions will automatically:
   - Build for all platforms
   - Create a GitHub Release
   - Attach binaries as release assets

## Local Cross-Compilation (Alternative)

### Windows (from macOS)

1. **Install Windows target**
   ```bash
   rustup target add x86_64-pc-windows-gnu
   brew install mingw-w64
   ```

2. **Build**
   ```bash
   cargo build --release --target x86_64-pc-windows-gnu
   ```

   Binary at: `target/x86_64-pc-windows-gnu/release/gitterm.exe`

### Linux (from macOS)

1. **Install Linux target**
   ```bash
   rustup target add x86_64-unknown-linux-gnu
   brew install filosottile/musl-cross/musl-cross
   ```

2. **Build**
   ```bash
   cargo build --release --target x86_64-unknown-linux-gnu
   ```

   Binary at: `target/x86_64-unknown-linux-gnu/release/gitterm`

## Dependencies

### macOS
- Xcode Command Line Tools
- Rust toolchain

### Windows (if building locally)
- Visual Studio Build Tools OR MinGW-w64
- Rust toolchain with MSVC or GNU target

### Linux (if building locally)
- Build essentials, pkg-config, cmake
- libxcb (shape, xfixes), libxkbcommon, libssl, libasound2, libclang, libxdo
- GTK3, Pango, ATK, GDK-Pixbuf (for file dialogs)
- libsoup-3.0, libwebkit2gtk-4.1 (for webview)
- Rust toolchain

## Notes

- The HTTP log server runs on `localhost:3030`
- All builds include the web-based log viewer
- macOS builds include native menu bar integration
- Windows/Linux builds use cross-platform menu fallbacks
