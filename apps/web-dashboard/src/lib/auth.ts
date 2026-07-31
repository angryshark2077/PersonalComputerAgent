import type { DashboardFetch } from "./api";

export interface BrowserSession {
  user: { id: string };
}

export async function getBrowserSession(
  fetcher: DashboardFetch,
  cloudApiOrigin: string,
): Promise<BrowserSession | null> {
  const response = await fetcher(apiUrl(cloudApiOrigin, "/api/auth/get-session"), {
    credentials: "include",
  });
  if (!response.ok) return null;
  const session = (await response.json().catch(() => null)) as BrowserSession | null;
  return typeof session?.user?.id === "string" && session.user.id.length > 0 ? session : null;
}

export function redirectToSignIn(): void {
  const callback = `${window.location.pathname}${window.location.search}`;
  window.location.assign(`/sign-in?callbackURL=${encodeURIComponent(callback)}`);
}

function apiUrl(cloudApiOrigin: string, path: string): string {
  return cloudApiOrigin.length === 0 ? path : new URL(path, cloudApiOrigin).toString();
}
