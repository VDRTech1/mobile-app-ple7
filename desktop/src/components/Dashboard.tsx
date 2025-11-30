import { useState, useEffect, useRef } from "react";
import { invoke } from "@tauri-apps/api/core";
import { getVersion } from "@tauri-apps/api/app";
import { LazyStore } from "@tauri-apps/plugin-store";
import { motion, AnimatePresence } from "framer-motion";
import {
  Network,
  Power,
  PowerOff,
  LogOut,
  ChevronDown,
  Check,
  Loader2,
  Globe,
  Shield,
  Wifi,
  WifiOff,
  RefreshCw,
  Router,
  Server,
} from "lucide-react";
import PleiadesLogo from "./PleiadesLogo";

interface NetworkData {
  id: string;
  name: string;
  description: string;
  ip_range: string;
}

interface Device {
  id: string;
  name: string;
  ip_address: string;
  public_key: string;
  is_online: boolean;
  is_exit_node: boolean;
  platform: string;
}

interface Relay {
  id: string;
  name: string;
  location: string;
  country_code: string;
  public_endpoint: string;
  status: string;
}

interface ExitNodeOption {
  id: string;
  name: string;
  type: "none" | "relay" | "device";
  countryCode?: string;
  icon?: string;
}

interface DashboardProps {
  onLogout: () => void;
}

type ConnectionStatus = "disconnected" | "connecting" | "connected" | "disconnecting";

// Country code to flag emoji
const countryToFlag = (code: string): string => {
  if (!code || code.length !== 2) return "🌐";
  const codePoints = code.toUpperCase().split("").map(c => 127397 + c.charCodeAt(0));
  return String.fromCodePoint(...codePoints);
};

export default function Dashboard({ onLogout }: DashboardProps) {
  const [networks, setNetworks] = useState<NetworkData[]>([]);
  const [selectedNetwork, setSelectedNetwork] = useState<NetworkData | null>(null);
  const [relays, setRelays] = useState<Relay[]>([]);
  const [exitNodes, setExitNodes] = useState<Device[]>([]);
  const [selectedExitNode, setSelectedExitNode] = useState<ExitNodeOption | null>(null);
  const [connectionStatus, setConnectionStatus] = useState<ConnectionStatus>("disconnected");
  const [showNetworkSelect, setShowNetworkSelect] = useState(false);
  const [showExitNodeSelect, setShowExitNodeSelect] = useState(false);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState("");
  const [appVersion, setAppVersion] = useState("");
  const [connectedDevice, setConnectedDevice] = useState<Device | null>(null);
  const pendingConnectChecked = useRef(false);

  // Check for pending connection on startup (after UAC elevation)
  const checkPendingConnection = async () => {
    if (pendingConnectChecked.current) return;
    pendingConnectChecked.current = true;

    try {
      const store = new LazyStore("pending.json");
      const pending = await store.get<{
        networkId: string;
        networkName: string;
        exitNode: ExitNodeOption | null;
      }>("pendingConnect");

      if (pending) {
        // Clear it immediately so we don't retry on next restart
        await store.delete("pendingConnect");
        await store.save();

        // Wait for networks to load, then auto-connect
        setTimeout(async () => {
          const network = networks.find(n => n.id === pending.networkId);
          if (network) {
            setSelectedNetwork(network);
            if (pending.exitNode) {
              setSelectedExitNode(pending.exitNode);
            }
            // Small delay to ensure state is set
            setTimeout(() => {
              handleConnect();
            }, 500);
          }
        }, 1000);
      }
    } catch (e) {
      // Ignore errors - store may not exist
    }
  };

  useEffect(() => {
    loadNetworks();
    loadRelays();
    getVersion().then(v => {
      setAppVersion(v);
    }).catch(() => {});
  }, []);

  // Check for pending connection after networks are loaded
  useEffect(() => {
    if (networks.length > 0 && !pendingConnectChecked.current) {
      checkPendingConnection();
    }
  }, [networks]);

  useEffect(() => {
    if (selectedNetwork) {
      loadExitNodes(selectedNetwork.id);
    }
  }, [selectedNetwork]);

  const loadNetworks = async () => {
    try {
      setLoading(true);
      const data = await invoke<NetworkData[]>("get_networks");
      setNetworks(data);
      if (data.length > 0) {
        setSelectedNetwork(data[0]);
      }
    } catch (err: any) {
      setError(err.toString());
    } finally {
      setLoading(false);
    }
  };

  const loadRelays = async () => {
    try {
      const data = await invoke<Relay[]>("get_relays");
      setRelays(data);
    } catch (err: any) {
      console.error("Failed to load relays:", err);
    }
  };

  const loadExitNodes = async (networkId: string) => {
    try {
      const devices = await invoke<Device[]>("get_devices", { networkId });
      // Filter to only show devices that can be exit nodes (routers, firewalls, servers)
      const exitNodeDevices = devices.filter(d =>
        d.is_exit_node && ["ROUTER", "FIREWALL", "SERVER"].includes(d.platform)
      );
      setExitNodes(exitNodeDevices);

      // Set default to "None" if no selection
      if (!selectedExitNode) {
        setSelectedExitNode({ id: "none", name: "None (mesh only)", type: "none" });
      }
    } catch (err: any) {
      console.error("Failed to load exit nodes:", err);
    }
  };

  const getDeviceName = (): string => {
    // Try to get a meaningful device name
    const platform = navigator.platform || "Unknown";
    if (platform.includes("Win")) return "Windows PC";
    if (platform.includes("Mac")) return "Mac";
    if (platform.includes("Linux")) return "Linux PC";
    return "Desktop";
  };

  const handleConnect = async () => {
    if (!selectedNetwork) {
      return;
    }

    // Save pending connection state BEFORE attempting (in case UAC triggers restart)
    try {
      const store = new LazyStore("pending.json");
      await store.set("pendingConnect", {
        networkId: selectedNetwork.id,
        networkName: selectedNetwork.name,
        exitNode: selectedExitNode,
      });
      await store.save();
    } catch (e) {
      // Ignore - store save is optional
    }

    setConnectionStatus("connecting");
    setError("");

    try {
      // Auto-register this device
      const deviceName = getDeviceName();
      const device = await invoke<Device>("auto_register_device", {
        networkId: selectedNetwork.id,
        deviceName,
      });

      setConnectedDevice(device);

      // Set exit node on backend (triggers relay VRF/WG configuration)
      const exitNodeType = selectedExitNode?.type || "none";
      const exitNodeId = selectedExitNode?.type !== "none" ? selectedExitNode?.id : null;
      await invoke("set_exit_node", {
        networkId: selectedNetwork.id,
        exitType: exitNodeType,
        exitId: exitNodeId,
      });

      // Connect VPN
      await invoke("connect_vpn", {
        deviceId: device.id,
        networkId: selectedNetwork.id,
        exitNodeType,
        exitNodeId,
      });

      setConnectionStatus("connected");

      // Clear pending connection state on success
      try {
        const store = new LazyStore("pending.json");
        await store.delete("pendingConnect");
        await store.save();
      } catch (e) {
        // Ignore
      }
    } catch (err: any) {
      setError(err.toString());
      setConnectionStatus("disconnected");

      // Clear pending connection state on error
      try {
        const store = new LazyStore("pending.json");
        await store.delete("pendingConnect");
        await store.save();
      } catch (e) {
        // Ignore
      }
    }
  };

  const handleDisconnect = async () => {
    setConnectionStatus("disconnecting");
    try {
      await invoke("disconnect_vpn");

      // Clear exit node on backend (triggers relay VRF/WG cleanup)
      if (selectedNetwork) {
        try {
          await invoke("set_exit_node", {
            networkId: selectedNetwork.id,
            exitType: "none",
            exitId: null,
          });
        } catch (e) {
          // Don't fail disconnect if exit node clear fails
          console.warn("Failed to clear exit node:", e);
        }
      }

      setConnectionStatus("disconnected");
      setConnectedDevice(null);
    } catch (err: any) {
      setError(err.toString());
      setConnectionStatus("connected");
    }
  };

  const isConnected = connectionStatus === "connected";
  const isConnecting = connectionStatus === "connecting" || connectionStatus === "disconnecting";

  return (
    <div className="min-h-screen p-4 flex flex-col">
      {/* Header */}
      <div className="flex items-center justify-between mb-6">
        <div className="flex items-center gap-2">
          <PleiadesLogo className="w-6 h-6 text-primary" />
          <div className="flex items-center gap-0.5 text-lg font-bold">
            <span className="text-primary">P</span>
            <span>LE</span>
            <span className="text-primary">7</span>
          </div>
        </div>
        <button
          onClick={onLogout}
          className="p-2 rounded-lg text-muted-foreground hover:text-foreground hover:bg-muted transition-colors"
          title="Sign out"
        >
          <LogOut className="w-5 h-5" />
        </button>
      </div>

      {/* Connection Status Card */}
      <motion.div
        initial={{ opacity: 0, y: 20 }}
        animate={{ opacity: 1, y: 0 }}
        className={`rounded-2xl border p-6 mb-4 ${
          isConnected
            ? "bg-green-500/10 border-green-500/30"
            : "bg-card"
        }`}
      >
        <div className="flex items-center justify-between mb-4">
          <div className="flex items-center gap-3">
            {isConnected ? (
              <div className="w-12 h-12 rounded-full bg-green-500/20 flex items-center justify-center">
                <Wifi className="w-6 h-6 text-green-500" />
              </div>
            ) : (
              <div className="w-12 h-12 rounded-full bg-muted flex items-center justify-center">
                <WifiOff className="w-6 h-6 text-muted-foreground" />
              </div>
            )}
            <div>
              <p className="font-semibold text-lg">
                {isConnected
                  ? "Connected"
                  : connectionStatus === "connecting"
                  ? "Connecting..."
                  : connectionStatus === "disconnecting"
                  ? "Disconnecting..."
                  : "Disconnected"}
              </p>
              {isConnected && connectedDevice && (
                <p className="text-sm text-muted-foreground">
                  {connectedDevice.ip_address}
                </p>
              )}
            </div>
          </div>

          <button
            onClick={isConnected ? handleDisconnect : handleConnect}
            disabled={isConnecting || !selectedNetwork}
            className={`w-16 h-16 rounded-full flex items-center justify-center transition-all ${
              isConnected
                ? "bg-green-500 hover:bg-green-600"
                : "bg-primary hover:opacity-90"
            } disabled:opacity-50`}
          >
            {isConnecting ? (
              <Loader2 className="w-8 h-8 text-white animate-spin" />
            ) : isConnected ? (
              <PowerOff className="w-8 h-8 text-white" />
            ) : (
              <Power className="w-8 h-8 text-white" />
            )}
          </button>
        </div>

        {isConnected && (
          <div className="grid grid-cols-2 gap-3">
            <div className="p-3 rounded-xl bg-background/50">
              <div className="flex items-center gap-2 text-sm text-muted-foreground mb-1">
                <Shield className="w-4 h-4" />
                <span>Encryption</span>
              </div>
              <p className="font-medium">WireGuard</p>
            </div>
            <div className="p-3 rounded-xl bg-background/50">
              <div className="flex items-center gap-2 text-sm text-muted-foreground mb-1">
                <Globe className="w-4 h-4" />
                <span>Network</span>
              </div>
              <p className="font-medium">{selectedNetwork?.name || "-"}</p>
            </div>
          </div>
        )}
      </motion.div>

      {/* Network Selector */}
      <motion.div
        initial={{ opacity: 0, y: 20 }}
        animate={{ opacity: 1, y: 0 }}
        transition={{ delay: 0.1 }}
        className="mb-4"
      >
        <label className="block text-sm font-medium text-muted-foreground mb-2">
          Network
        </label>
        <div className="relative">
          <button
            onClick={() => setShowNetworkSelect(!showNetworkSelect)}
            disabled={isConnected}
            className="w-full flex items-center justify-between p-3 bg-card border rounded-xl hover:bg-muted/50 transition-colors disabled:opacity-50"
          >
            <div className="flex items-center gap-3">
              <Network className="w-5 h-5 text-primary" />
              <span>{selectedNetwork?.name || "Select network"}</span>
            </div>
            <ChevronDown className={`w-4 h-4 transition-transform ${showNetworkSelect ? "rotate-180" : ""}`} />
          </button>

          <AnimatePresence>
            {showNetworkSelect && (
              <motion.div
                initial={{ opacity: 0, y: -10 }}
                animate={{ opacity: 1, y: 0 }}
                exit={{ opacity: 0, y: -10 }}
                className="absolute top-full left-0 right-0 mt-1 bg-card border rounded-xl shadow-lg overflow-hidden z-10"
              >
                {networks.map((network) => (
                  <button
                    key={network.id}
                    onClick={() => {
                      setSelectedNetwork(network);
                      setShowNetworkSelect(false);
                    }}
                    className="w-full flex items-center justify-between p-3 hover:bg-muted/50 transition-colors"
                  >
                    <span>{network.name}</span>
                    {selectedNetwork?.id === network.id && (
                      <Check className="w-4 h-4 text-primary" />
                    )}
                  </button>
                ))}
              </motion.div>
            )}
          </AnimatePresence>
        </div>
      </motion.div>

      {/* Exit Node Selector */}
      <motion.div
        initial={{ opacity: 0, y: 20 }}
        animate={{ opacity: 1, y: 0 }}
        transition={{ delay: 0.2 }}
        className="mb-4"
      >
        <label className="block text-sm font-medium text-muted-foreground mb-2">
          Exit Node
        </label>
        <div className="relative">
          <button
            onClick={() => setShowExitNodeSelect(!showExitNodeSelect)}
            disabled={isConnected}
            className="w-full flex items-center justify-between p-3 bg-card border rounded-xl hover:bg-muted/50 transition-colors disabled:opacity-50"
          >
            <div className="flex items-center gap-3">
              {selectedExitNode?.type === "relay" ? (
                <span className="text-lg">{countryToFlag(selectedExitNode.countryCode || "")}</span>
              ) : selectedExitNode?.type === "device" ? (
                <Router className="w-5 h-5 text-primary" />
              ) : (
                <Globe className="w-5 h-5 text-muted-foreground" />
              )}
              <span>{selectedExitNode?.name || "Select exit node"}</span>
            </div>
            <ChevronDown className={`w-4 h-4 transition-transform ${showExitNodeSelect ? "rotate-180" : ""}`} />
          </button>

          <AnimatePresence>
            {showExitNodeSelect && (
              <motion.div
                initial={{ opacity: 0, y: -10 }}
                animate={{ opacity: 1, y: 0 }}
                exit={{ opacity: 0, y: -10 }}
                className="absolute top-full left-0 right-0 mt-1 bg-card border rounded-xl shadow-lg overflow-hidden z-10 max-h-64 overflow-y-auto"
              >
                {/* None option */}
                <button
                  onClick={async () => {
                    setSelectedExitNode({ id: "none", name: "None (mesh only)", type: "none" });
                    setShowExitNodeSelect(false);
                    if (selectedNetwork) {
                      try {
                        await invoke("set_exit_node", {
                          networkId: selectedNetwork.id,
                          exitType: "none",
                          exitId: null,
                        });
                      } catch (err) {
                        console.error("Failed to set exit node:", err);
                      }
                    }
                  }}
                  className="w-full flex items-center justify-between p-3 hover:bg-muted/50 transition-colors"
                >
                  <div className="flex items-center gap-3">
                    <Globe className="w-5 h-5 text-muted-foreground" />
                    <span>None (mesh only)</span>
                  </div>
                  {selectedExitNode?.id === "none" && (
                    <Check className="w-4 h-4 text-primary" />
                  )}
                </button>

                {/* Relays section */}
                {relays.length > 0 && (
                  <>
                    <div className="px-3 py-2 text-xs font-medium text-muted-foreground bg-muted/30">
                      PLE7 Relays
                    </div>
                    {relays.map((relay) => (
                      <button
                        key={relay.id}
                        onClick={async () => {
                          setSelectedExitNode({
                            id: relay.id,
                            name: relay.location,
                            type: "relay",
                            countryCode: relay.country_code,
                          });
                          setShowExitNodeSelect(false);
                          if (selectedNetwork) {
                            try {
                              // Setting relay as exit also changes mesh hub
                              await invoke("set_exit_node", {
                                networkId: selectedNetwork.id,
                                exitType: "relay",
                                exitId: relay.id,
                              });
                            } catch (err) {
                              console.error("Failed to set exit node:", err);
                            }
                          }
                        }}
                        className="w-full flex items-center justify-between p-3 hover:bg-muted/50 transition-colors"
                      >
                        <div className="flex items-center gap-3">
                          <span className="text-lg">{countryToFlag(relay.country_code)}</span>
                          <span>{relay.location}</span>
                        </div>
                        {selectedExitNode?.id === relay.id && (
                          <Check className="w-4 h-4 text-primary" />
                        )}
                      </button>
                    ))}
                  </>
                )}

                {/* User devices section */}
                {exitNodes.length > 0 && (
                  <>
                    <div className="px-3 py-2 text-xs font-medium text-muted-foreground bg-muted/30">
                      Your Devices
                    </div>
                    {exitNodes.map((device) => (
                      <button
                        key={device.id}
                        onClick={async () => {
                          setSelectedExitNode({
                            id: device.id,
                            name: device.name,
                            type: "device",
                          });
                          setShowExitNodeSelect(false);
                          if (selectedNetwork) {
                            try {
                              await invoke("set_exit_node", {
                                networkId: selectedNetwork.id,
                                exitType: "device",
                                exitId: device.id,
                              });
                            } catch (err) {
                              console.error("Failed to set exit node:", err);
                            }
                          }
                        }}
                        className="w-full flex items-center justify-between p-3 hover:bg-muted/50 transition-colors"
                      >
                        <div className="flex items-center gap-3">
                          {device.platform === "ROUTER" ? (
                            <Router className="w-5 h-5 text-primary" />
                          ) : (
                            <Server className="w-5 h-5 text-primary" />
                          )}
                          <div className="text-left">
                            <span>{device.name}</span>
                            <p className="text-xs text-muted-foreground">{device.ip_address}</p>
                          </div>
                        </div>
                        {selectedExitNode?.id === device.id && (
                          <Check className="w-4 h-4 text-primary" />
                        )}
                      </button>
                    ))}
                  </>
                )}
              </motion.div>
            )}
          </AnimatePresence>
        </div>
      </motion.div>

      {/* Refresh Button */}
      <motion.button
        initial={{ opacity: 0 }}
        animate={{ opacity: 1 }}
        transition={{ delay: 0.3 }}
        onClick={() => {
          loadNetworks();
          loadRelays();
        }}
        disabled={loading}
        className="w-full flex items-center justify-center gap-2 p-3 text-muted-foreground hover:text-foreground hover:bg-muted/50 rounded-xl transition-colors"
      >
        <RefreshCw className={`w-4 h-4 ${loading ? "animate-spin" : ""}`} />
        <span>Refresh</span>
      </motion.button>

      {/* Error Display */}
      {error && (
        <motion.div
          initial={{ opacity: 0 }}
          animate={{ opacity: 1 }}
          className="mt-4 p-3 rounded-xl bg-destructive/10 text-destructive text-sm"
        >
          {error}
        </motion.div>
      )}

      {/* Footer - positioned at bottom */}
      <div className="mt-auto pt-6 pb-2 flex items-center justify-center gap-2 text-xs text-muted-foreground/50">
        {appVersion && <span>v{appVersion}</span>}
        {appVersion && <span>•</span>}
        <span>&copy; 2025 PLE7</span>
      </div>

    </div>
  );
}
