# Claude Development Guidelines - PLE7 Apps

## Repository Overview
Desktop and mobile apps for PLE7 VPN platform.
- **Desktop**: `/desktop/` - Tauri 2.0 app (Rust + React)
- **Mobile**: Coming soon

## Critical Rules

### Platform-Specific Resources
**NEVER add platform-specific resources to `tauri.conf.json` directly.**

The GitHub Actions workflow (`build-desktop.yml`) handles platform-specific bundling:
- **macOS**: Adds `ple7-helper` and plist via "Configure macOS resources" step
- **Windows**: Adds `wintun.dll` via "Configure Windows resources" step
- **Linux**: No special resources

If you add Windows-only files (like `wintun.dll`) to the base config, Linux and macOS builds will fail because those files don't exist on those platforms.

### Build Process
Builds are done via GitHub Actions, not locally. To release:
1. Make changes
2. Update version in both `tauri.conf.json` AND `Cargo.toml`
3. Commit and push to main
4. Create and push a tag: `git tag v0.x.x && git push origin v0.x.x`
5. GitHub Actions builds all platforms automatically

### Platform Differences
| Platform | TUN Implementation | Special Files |
|----------|-------------------|---------------|
| macOS | Helper daemon (`ple7-helper`) via Unix socket | `ple7-helper`, `com.ple7.vpn.helper.plist` |
| Windows | Wintun driver | `wintun.dll` |
| Linux | `tun` crate directly | None |

### Code Organization
- `src/tun_device.rs` - Platform-specific TUN code with `#[cfg(target_os = "...")]`
- `src/tunnel.rs` - Cross-platform tunnel manager
- `src/wireguard.rs` - WireGuard implementation using boringtun
- `helper/` - macOS privileged helper daemon (separate binary)
