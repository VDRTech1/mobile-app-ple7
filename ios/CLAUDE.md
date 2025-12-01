# Claude Development Guidelines - PLE7 iOS App

## Project Overview
Native iOS VPN client for PLE7 mesh networks using SwiftUI and WireGuardKit.

**Bundle IDs:**
- Main app: `com.ple7.vpn`
- Network Extension: `com.ple7.vpn.PacketTunnel`
- App Group: `group.com.ple7.vpn`

**Backend API:** `https://ple7.com/api`

## Project Structure

```
ios/
├── project.yml              # XcodeGen configuration
├── PLE7/                    # Main app target
│   ├── App/PLE7App.swift    # App entry point
│   ├── Views/               # SwiftUI views
│   │   ├── ContentView.swift
│   │   ├── LoginView.swift
│   │   ├── MainView.swift
│   │   └── NetworkDetailView.swift
│   ├── ViewModels/
│   │   └── NetworksViewModel.swift
│   ├── Services/
│   │   ├── APIClient.swift      # Backend API communication
│   │   ├── AuthManager.swift    # Authentication state
│   │   └── VPNManager.swift     # NEVPNManager wrapper
│   ├── Models/Models.swift      # Data models
│   ├── PLE7.entitlements
│   ├── Info.plist
│   └── Assets.xcassets/
├── PacketTunnel/            # Network Extension target
│   ├── PacketTunnelProvider.swift  # WireGuard tunnel implementation
│   ├── PacketTunnel.entitlements
│   └── Info.plist
└── Shared/                  # Shared code between targets
```

## Quick Commands

```bash
# Generate Xcode project (after any project.yml changes)
xcodegen generate

# Open project
open PLE7.xcodeproj

# Clean build
rm -rf ~/Library/Developer/Xcode/DerivedData/PLE7-*
```

## Architecture

### Main App
- **SwiftUI** for UI
- **AuthManager**: Handles login/logout, token storage in Keychain
- **VPNManager**: Controls VPN via NEVPNManager, stores config in shared Keychain
- **APIClient**: REST API communication with backend

### PacketTunnel Extension
- **NEPacketTunnelProvider** subclass
- Uses **WireGuardKit** for tunnel implementation
- Reads WireGuard config from shared Keychain (App Group)
- Runs as separate process from main app

### Data Flow
```
Main App                          PacketTunnel Extension
    │                                      │
    ├─► APIClient.getDeviceConfig()        │
    │         │                            │
    │         ▼                            │
    ├─► Store in shared Keychain ─────────►│
    │         │                            │
    │         ▼                            │
    ├─► VPNManager.connect()               │
    │         │                            │
    │         ▼                            │
    │   NEVPNManager.startVPNTunnel() ────►│
    │                                      ▼
    │                          PacketTunnelProvider.startTunnel()
    │                                      │
    │                                      ▼
    │                          Read config from Keychain
    │                                      │
    │                                      ▼
    │                          WireGuardAdapter.start()
```

## API Endpoints Used

```
POST /api/auth/login          - Email/password login
POST /api/auth/register       - Create account
GET  /api/auth/me             - Get current user
GET  /api/auth/google/mobile  - Google OAuth (opens in browser)

GET  /api/mesh/networks                    - List networks
GET  /api/mesh/networks/:id/devices        - List devices in network
GET  /api/mesh/devices/:id/config          - Get WireGuard config for device
POST /api/mesh/networks/:id/devices        - Register new device
```

## Models

```swift
Network: id, name, description, ipRange, deviceCount
Device: id, name, ip, platform, publicKey, isExitNode, networkId
User: id, email, plan, emailVerified
WireGuardConfig: privateKey, address, dns, peers[]
WireGuardPeer: publicKey, allowedIPs, endpoint, persistentKeepalive
```

## Dependencies (via Swift Package Manager)

- **WireGuardKit** (github.com/WireGuard/wireguard-apple) - WireGuard implementation
- **KeychainAccess** (github.com/kishikawakatsumi/KeychainAccess) - Keychain wrapper

## Entitlements Required

**Main App (PLE7.entitlements):**
- `com.apple.developer.networking.networkextension` → packet-tunnel-provider
- `com.apple.developer.networking.vpn.api` → allow-vpn
- `com.apple.security.application-groups` → group.com.ple7.vpn
- `keychain-access-groups`

**Extension (PacketTunnel.entitlements):**
- `com.apple.developer.networking.networkextension` → packet-tunnel-provider
- `com.apple.security.application-groups` → group.com.ple7.vpn
- `keychain-access-groups`

## Common Tasks

### Add a new View
1. Create SwiftUI view in `PLE7/Views/`
2. Add navigation from parent view
3. Use `@EnvironmentObject` for AuthManager/VPNManager access

### Add API endpoint
1. Add method to `APIClient.swift`
2. Add response model to `Models.swift` if needed
3. Call from ViewModel or Service

### Modify WireGuard config handling
1. Update `WireGuardConfig` model in both:
   - `PLE7/Models/Models.swift`
   - `PacketTunnel/PacketTunnelProvider.swift`
2. Update `buildWireGuardConfig()` in PacketTunnelProvider

### Debug VPN issues
1. Connect iPhone to Mac
2. Open Console.app
3. Filter by "com.ple7.vpn.PacketTunnel"
4. Check for WireGuard adapter errors

## Build Requirements

- macOS with Xcode 15+
- Go installed (`brew install go`) - required for WireGuardKit
- XcodeGen (`brew install xcodegen`)
- Physical iPhone for testing (VPN doesn't work in simulator)

## Related Repositories

- **Backend/Frontend:** github.com/vji11/ple7 (`/opt/apps/ple7/`)
- **Desktop App:** This repo, `/desktop/` folder

## Current Status

- ✅ Project structure created
- ✅ SwiftUI views (Login, Main, NetworkDetail)
- ✅ API client with auth
- ✅ VPN manager with NEVPNManager
- ✅ PacketTunnel extension with WireGuardKit
- ⏳ Testing on device
- ⏳ App icon
- ⏳ App Store submission
