import { useState, useEffect } from "react";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { onOpenUrl } from "@tauri-apps/plugin-deep-link";
import Login from "./components/Login";
import Dashboard from "./components/Dashboard";

interface User {
  id: string;
  email: string;
  plan: string;
}

function App() {
  const [user, setUser] = useState<User | null>(null);
  const [loading, setLoading] = useState(true);

  // Handle deep link URL (OAuth callback)
  const handleDeepLink = async (url: string) => {
    console.log("Deep link received:", url);
    try {
      const urlObj = new URL(url);
      const token = urlObj.searchParams.get("token");
      if (token) {
        // Store the token
        await invoke("store_token", { token });
        // Verify and get user data
        const userData = await invoke<User>("verify_token", { token });
        setUser(userData);
      }
    } catch (error) {
      console.error("Error handling deep link:", error);
    }
  };

  useEffect(() => {
    checkAuth();

    // Listen for deep links on macOS (while app is running)
    const unsubscribeDeepLink = onOpenUrl((urls) => {
      urls.forEach((url) => handleDeepLink(url));
    });

    // Listen for deep-link events from single-instance plugin (Windows/Linux)
    const unsubscribeEvent = listen<string>("deep-link", (event) => {
      handleDeepLink(event.payload);
    });

    return () => {
      unsubscribeDeepLink.then((fn) => fn());
      unsubscribeEvent.then((fn) => fn());
    };
  }, []);

  const checkAuth = async () => {
    try {
      const storedToken = await invoke<string | null>("get_stored_token");
      if (storedToken) {
        const userData = await invoke<User>("verify_token", { token: storedToken });
        setUser(userData);
      }
    } catch (error) {
      console.log("No valid session");
    } finally {
      setLoading(false);
    }
  };

  const handleLogin = (userData: User) => {
    setUser(userData);
  };

  const handleLogout = async () => {
    try {
      await invoke("clear_stored_token");
    } catch (error) {
      console.error("Error clearing token:", error);
    }
    setUser(null);
  };

  if (loading) {
    return (
      <div className="flex items-center justify-center h-screen">
        <div className="flex flex-col items-center gap-3">
          <div className="w-8 h-8 border-2 border-primary border-t-transparent rounded-full animate-spin" />
          <p className="text-sm text-muted-foreground">Loading...</p>
        </div>
      </div>
    );
  }

  return (
    <div className="min-h-screen">
      {user ? (
        <Dashboard user={user} onLogout={handleLogout} />
      ) : (
        <Login onLogin={handleLogin} />
      )}
    </div>
  );
}

export default App;
