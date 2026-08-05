"use client";

import Link from "next/link";
import { usePathname } from "next/navigation";
import type { ReactNode } from "react";
import { useState } from "react";

import { cloudApiOrigin } from "../lib/api";
import { signOut } from "../lib/auth";

export function DashboardShell({ children }: { children: ReactNode }) {
  const pathname = usePathname();
  const chatsActive = pathname === "/chats" || pathname.includes("/chats/");
  const messagesActive = pathname === "/messages" || pathname.includes("/messages/");
  const devicesActive = !chatsActive && !messagesActive && (pathname.startsWith("/devices") || pathname === "/");
  const [signingOut, setSigningOut] = useState(false);
  const [signOutError, setSignOutError] = useState<string | null>(null);

  async function leaveDashboard(): Promise<void> {
    setSigningOut(true);
    setSignOutError(null);
    try {
      await signOut(window.fetch, cloudApiOrigin());
      window.location.assign("/sign-in");
    } catch {
      setSignOutError("Unable to sign out. Please try again.");
      setSigningOut(false);
    }
  }

  return (
    <div className="dashboard-shell">
      <header className="dashboard-nav">
        <Link className="dashboard-brand" href="/">Personal Computer Agent</Link>
        <nav aria-label="Dashboard">
          <Link className={devicesActive ? "is-active" : undefined} href="/">
            Devices
          </Link>
          <Link className={chatsActive ? "is-active" : undefined} href="/chats">
            WeChat
          </Link>
          <Link className={messagesActive ? "is-active" : undefined} href="/messages">
            Messages
          </Link>
        </nav>
        <div className="dashboard-account">
          {signOutError !== null ? <p role="alert">{signOutError}</p> : null}
          <button type="button" className="quiet-button" disabled={signingOut} onClick={() => void leaveDashboard()}>
            {signingOut ? "Signing out…" : "Sign out"}
          </button>
        </div>
      </header>
      <main className="dashboard-content">{children}</main>
    </div>
  );
}
