# Building GitTerm

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
  libatk1.0-dev libgdk-pixbuf2.0-dev
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
cargo build --release
```

Binary at: `target/release/gitterm`

### Running

```bash
./target/release/gitterm
```

## Cross-Platform Builds (GitHub Actions)

### Prerequisites
Before GitHub Actions can build, you need to publish your `iced_term_fork` changes:

1. **Fork iced_term to your GitHub account**
   ```bash
   cd ../iced_term_fork
   git remote add myfork https://github.com/YOUR_USERNAME/iced_term.git
   git push myfork master
   ```

2. **Update Cargo.toml to use git dependency**
   ```toml
   # Change from:
   iced_term = { path = "../iced_term_fork" }

   # To:
   iced_term = { git = "https://github.com/YOUR_USERNAME/iced_term.git", branch = "master" }
   ```

3. **Push to GitHub**
   ```bash
   git add Cargo.toml
   git commit -m "Use git dependency for iced_term"
   git push
   ```

### Triggering Builds

GitHub Actions will automatically build for all platforms on:
- Push to `master` or `main` branch
- Creating a tag (e.g., `v1.0.0`)
- Manual workflow dispatch

**Platforms built:**
- macOS (x86_64 Intel & aarch64 Apple Silicon) - `.app` bundle
- Windows (x86_64) - `.exe`
- Linux (x86_64) - binary

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
- libxcb (shape, xfixes), libxkbcommon, libssl, libasound2, libclang
- GTK3, Pango, ATK, GDK-Pixbuf (for file dialogs)
- Rust toolchain

## Notes

- The HTTP log server runs on `localhost:3030`
- All builds include the web-based log viewer
- macOS builds include native menu bar integration
- Windows/Linux builds use cross-platform menu fallbacks
