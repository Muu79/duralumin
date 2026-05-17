## [0.2.0-rc.1] - 2026-05-16

### 🐛 Bug Fixes

- *(db)* Split migrations into up-down format for save version downgrades

### 💼 Other

- Implimneted dynamic rules and rss restreaming of matched epissodes
- Added TUI for interactive UI
- Project re-structure: Cli split into command modules
- Clippy linting
- V0.2.0 bump

### ⚙️ Miscellaneous Tasks

- Added git-cliff changelog tracking
- Enabled build-caching for release dist
- *(deb)* Added arch64 deb support and moved deb build to dist extra-artifacts build
## [0.1.3-rc.1] - 2026-05-14

### 💼 Other

- Document ReadWritePaths customisation
- Reimport, duplicate prevention, permission fixes, auth logging
- Added feed slug aliases and global download speed rate limter
- Bump v0.1.3-rc.1
## [0.1.1-deb-test.1] - 2026-05-10

### 🐛 Bug Fixes

- Fix cargo-deb asset paths and workflow upload paths

### 💼 Other

- Merge branch 'main' into dev
- Bump version to 0.1.1-deb-test for all packages in Cargo.lock and Cargo.toml
- Bump version to 0.1.1-deb-test.1 in Cargo.toml and Cargo.lock; update upload paths in build-deb.yml
- Add maintainer scripts, system user, default config, fix service binary path
## [0.1.0-rc.1] - 2026-05-10

### 🐛 Bug Fixes

- Fix i686-windows build: define __SSE2__ for clang-cl via CFLAGS

aws-lc-sys's internal.h guards x86 assembly on __SSE2__, but cargo-xwin's
clang-cl doesn't set it even when -arch:SSE2 is passed. Setting
CFLAGS_i686_pc_windows_msvc in .cargo/config.toml injects -D__SSE2__ only
for that target, satisfying the preprocessor check without disabling asm.

Co-Authored-By: Claude Sonnet 4.6 <noreply@anthropic.com>
- Fix i686-windows: NO_ASM cmake build to avoid NASM PATH timing issues

### 💼 Other

- Added more ISA coverage and deb file
- Startup now creates storage directories if missing
- Bump for rc.1
- Dist config updates
- Update readme path and add License.rtf for installer
- Adjusted build targets
- Add NASM as system dependency for i686-win runner
- Update .gitignore to include additional files and corrected dist system deps section
- Update dist-workspace.toml to configure i686-pc-windows-msvc runner for native builds
- Add NASM as a build dependency for i686-pc-windows-msvc runner
- Update NASM dependency configuration for i686-pc-windows-msvc target
- Remove version specification for NASM dependency in i686-pc-windows-msvc
- Remove NO_ASM workaround; windows-2022 runner has NASM pre-installed
- Switch TLS backend from aws-lc-rs to ring
## [unreleased]

### 🐛 Bug Fixes

- *(db)* Split migrations into up-down format for save version downgrades

### 💼 Other

- Implimneted dynamic rules and rss restreaming of matched epissodes
- Added TUI for interactive UI
- Project re-structure: Cli split into command modules
- Clippy linting
- V0.2.0 bump

### ⚙️ Miscellaneous Tasks

- Added git-cliff changelog tracking
- Enabled build-caching for release dist
- *(deb)* Added arch64 deb support and moved deb build to dist extra-artifacts build
## [0.1.3-rc.1] - 2026-05-14

### 💼 Other

- Document ReadWritePaths customisation
- Reimport, duplicate prevention, permission fixes, auth logging
- Added feed slug aliases and global download speed rate limter
- Bump v0.1.3-rc.1
## [0.1.1-deb-test.1] - 2026-05-10

### 🐛 Bug Fixes

- Fix priority docs and config example to match lower-first semantics
- Fix TOML: use dotted keys for match sub-table instead of [header] syntax
- Fix cargo-deb asset paths and workflow upload paths

### 💼 Other

- Restore build-deb job and attach service file to releases
- Add standalone deb build workflow; revert release.yml patch
- Update to public email
- Merge branch 'main' into dev
- Bump version to 0.1.1-deb-test for all packages in Cargo.lock and Cargo.toml
- Bump version to 0.1.1-deb-test.1 in Cargo.toml and Cargo.lock; update upload paths in build-deb.yml
- Add maintainer scripts, system user, default config, fix service binary path
## [0.1.1] - 2026-05-10

### 💼 Other

- Refactor project configuration and dependencies

- switch from aws-lc-rs to rustls for TLS support
- add windows installer configuration using wix
- added windows i686 target for 32-bit support
- added Linux musl target for Arch64, ArchV7, and x86
## [0.1.0-rc.1] - 2026-05-10

### 🐛 Bug Fixes

- Fix i686-windows build: define __SSE2__ for clang-cl via CFLAGS

aws-lc-sys's internal.h guards x86 assembly on __SSE2__, but cargo-xwin's
clang-cl doesn't set it even when -arch:SSE2 is passed. Setting
CFLAGS_i686_pc_windows_msvc in .cargo/config.toml injects -D__SSE2__ only
for that target, satisfying the preprocessor check without disabling asm.

Co-Authored-By: Claude Sonnet 4.6 <noreply@anthropic.com>
- Fix i686-windows: NO_ASM cmake build to avoid NASM PATH timing issues

### 💼 Other

- Added more ISA coverage and deb file
- Startup now creates storage directories if missing
- Bump for rc.1
- Dist config updates
- Update readme path and add License.rtf for installer
- Adjusted build targets
- Add NASM as system dependency for i686-win runner
- Update .gitignore to include additional files and corrected dist system deps section
- Update dist-workspace.toml to configure i686-pc-windows-msvc runner for native builds
- Add NASM as a build dependency for i686-pc-windows-msvc runner
- Update NASM dependency configuration for i686-pc-windows-msvc target
- Remove version specification for NASM dependency in i686-pc-windows-msvc
- Remove NO_ASM workaround; windows-2022 runner has NASM pre-installed
- Switch TLS backend from aws-lc-rs to ring
## [0.1.0] - 2026-05-09

### 🚀 Features

- Add image_url field to Feed and FeedMeta structs
- Add HTTP server with authentication and RSS feed handling (v0.2)
- Add systemd service file for Duralumin daemon

### 🐛 Bug Fixes

- Fixed existing downloads re-downloading

### 💼 Other

- Init (checkpoint 1 + 2)
- Checkpoint 3-6
- Version 0.1 release
- Better explained config options
- Rename bin to dura
- Change config logging to only apply to daemon or server commads
- Pretty printing
- Better version handling
- Added dist automation on release
- README Skeleton
- Cli restructure (run + serve) -> start +
added delete and recheck
- Added clap completion command
- Pre-release documentation
- Cargo format + clippy
- Format internal-crates

### 🚜 Refactor

- Refactor to function
- Streamline config handling and enhance downloader configuration
