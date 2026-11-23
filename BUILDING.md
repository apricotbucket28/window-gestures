# Building Window Gestures

## Prerequisites

- Windows 11 on x64 or ARM64.
- Rust **1.95 or newer** with the Windows MSVC toolchain. [Install Rust with rustup](https://rustup.rs/).
- Visual Studio or Visual Studio Build Tools with the **Desktop development with C++** workload and a Windows SDK. For ARM64 builds, include the MSVC ARM64 C++ build tools. [Download Visual Studio or Build Tools](https://visualstudio.microsoft.com/downloads/).
- Windows App Runtime **2.6.4 or later in the 2.x series**, required by Reactor. [Download the runtime from Microsoft](https://learn.microsoft.com/en-us/windows/apps/windows-app-sdk/downloads#stable-release).

## Build and run

From the repository root, run these commands to launch a development build or compile an optimized executable for your native MSVC target.

```powershell
cargo run --locked
cargo build --locked --release
```

The release executable is `target/release/window-gestures.exe`. The app starts in the notification area. If you don't see its icon, check **Show hidden icons**.

To select a target explicitly, use one of the following Rust target triples. With `--target`, Cargo writes the executable to `target/<target>/release/window-gestures.exe`.

| Windows architecture | Rust target               |
| -------------------- | ------------------------- |
| x64                  | `x86_64-pc-windows-msvc`  |
| ARM64                | `aarch64-pc-windows-msvc` |

For example, to build the ARM64 executable on an x64 machine with the ARM64 C++ tools installed, run these commands.

```powershell
rustup target add aarch64-pc-windows-msvc
cargo build --locked --release --target aarch64-pc-windows-msvc
```

## Package a release

Install Git and PowerShell **7.4 or newer**, then add the Rust documentation component for the standard library license notices.

```powershell
rustup component add rust-docs
```

Start in the repository root with a clean working tree. The script packages the source at `HEAD`. Run the command for the architecture you want to distribute.

```powershell
./scripts/package-preview.ps1 -Architecture x64
./scripts/package-preview.ps1 -Architecture arm64
```

The first run for a commit creates `window-gestures-<version>-source.zip` in `target/packages/<revision>/`. Both architectures share that archive. Each run writes `window-gestures-<version>-<architecture>-portable.zip` and, if requested, `window-gestures-<architecture>-setup.exe` to the `x64/` or `arm64/` subdirectory. The script takes the version from `Cargo.toml` and prints SHA-256 hashes for the packages and the source ZIP.

`BUILD.txt` in the portable ZIP records the commit, Rust target, compiler version, and matching source archive. The source ZIP includes the locked dependencies. To rebuild it, extract it to a short path and run the command from the extracted directory. This example uses the x64 target. Use the target recorded in `BUILD.txt` for your archive.

```powershell
cargo build --frozen --release --target x86_64-pc-windows-msvc
```

## Build the installer

Install [Inno Setup](https://jrsoftware.org/inno-setup/downloads), then pass the path to `ISCC.exe` in the packaging command. Use `-Architecture arm64` for Windows on Arm.

```powershell
./scripts/package-preview.ps1 -Architecture x64 -InnoCompiler 'C:\path\to\ISCC.exe'
```

This produces a per-user setup executable alongside the ZIPs. Setup adds a Start menu shortcut and an Installed apps entry. It downloads and verifies the Microsoft runtime installer when a compatible runtime is missing. The shared runtime stays installed after uninstalling Window Gestures.

For public releases, bump the package version in `Cargo.toml` and publish both installers and portable ZIPs with the shared source archive. Keep the generated installer filenames so the README buttons can download them from the latest GitHub release. Use the installer URLs for the specific release tag and the matching SHA-256 hashes when submitting or updating a WinGet manifest.

## Update Reactor

The upstream Microsoft windows-rs revision is recorded in [windows-rs-revision.txt](third_party/reactor-popup/windows-rs-revision.txt). [upstream.patch](third_party/reactor-popup/upstream.patch) contains the changes applied to the snapshot. The popup implementation lives in [screen.rs](third_party/reactor-popup/screen.rs).

Run these commands from a clean checkout when updating to a new upstream release. Use a windows-rs tag or commit for `-Revision`.

```powershell
./scripts/update-reactor.ps1 -Revision 'tag-or-commit'
cargo check --all-targets
cargo test --locked --all-targets
```

The script stages the upstream files and validates the patch before replacing the snapshot. To verify an existing snapshot, run `./scripts/update-reactor.ps1 -Check`. Pass `-Upstream <local-windows-rs-checkout>` to use a local Git repository. See the [maintainer notes](third_party/README.md) for the purpose of each local change and the conflict resolution workflow.

Check `RUNTIME_VERSION` in Reactor's `src/native/bootstrap.rs` against the prerequisite above and the installer check in `scripts/window-gestures.iss`. Verify tray and window-action menus, Settings, and Quick Guide on Windows before shipping the update.
