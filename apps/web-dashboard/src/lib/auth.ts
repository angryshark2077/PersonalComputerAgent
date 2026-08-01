import type { DashboardFetch } from "./api";

export interface BrowserSession {
  user: { id: string };
}

export interface EmailCredentials {
  email: string;
  password: string;
}

export interface EmailRegistration extends EmailCredentials {
  name: string;
}

export class AuthenticationError extends Error {
  constructor(message: string) {
    super(message);
    this.name = "AuthenticationError";
  }
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

export function safeLocalCallbackPath(value: string | null): string {
  return value !== null && /^\/(?![\\/])/.test(value) ? value : "/";
}

export async function signInWithEmail(
  fetcher: DashboardFetch,
  cloudApiOrigin: string,
  credentials: EmailCredentials,
): Promise<void> {
  await emailRequest(fetcher, cloudApiOrigin, "/api/auth/sign-in/email", credentials);
}

export async function signUpWithEmail(
  fetcher: DashboardFetch,
  cloudApiOrigin: string,
  registration: EmailRegistration,
): Promise<void> {
  await emailRequest(fetcher, cloudApiOrigin, "/api/auth/sign-up/email", registration);
}

export async function signOut(fetcher: DashboardFetch, cloudApiOrigin: string): Promise<void> {
  const response = await fetcher(apiUrl(cloudApiOrigin, "/api/auth/sign-out"), {
    method: "POST",
    credentials: "include",
  });
  if (!response.ok) throw new AuthenticationError("Unable to sign out.");
}

function apiUrl(cloudApiOrigin: string, path: string): string {
  return cloudApiOrigin.length === 0 ? path : new URL(path, cloudApiOrigin).toString();
}

async function emailRequest(
  fetcher: DashboardFetch,
  cloudApiOrigin: string,
  path: string,
  body: EmailCredentials | EmailRegistration,
): Promise<void> {
  const response = await fetcher(apiUrl(cloudApiOrigin, path), {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify(body),
    credentials: "include",
  });
  if (response.ok) return;
  const result = (await response.json().catch(() => null)) as { message?: string } | null;
  throw new AuthenticationError(result?.message ?? "Authentication failed.");
}
