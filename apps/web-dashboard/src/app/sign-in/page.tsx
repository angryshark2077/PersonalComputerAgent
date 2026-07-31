"use client";

import { useSearchParams } from "next/navigation";
import { FormEvent, Suspense, useState } from "react";

import { cloudApiOrigin } from "../../lib/api";
import { AuthenticationError, safeLocalCallbackPath, signInWithEmail, signUpWithEmail } from "../../lib/auth";

export default function SignInPage() {
  return (
    <Suspense fallback={<main><p>Loading sign in…</p></main>}>
      <SignInScreen />
    </Suspense>
  );
}

function SignInScreen() {
  const params = useSearchParams();
  const callbackURL = safeLocalCallbackPath(params.get("callbackURL"));
  const [registering, setRegistering] = useState(false);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);

  async function submit(event: FormEvent<HTMLFormElement>): Promise<void> {
    event.preventDefault();
    const form = new FormData(event.currentTarget);
    const email = String(form.get("email") ?? "");
    const password = String(form.get("password") ?? "");
    const name = String(form.get("name") ?? "");
    setBusy(true);
    setError(null);
    try {
      if (registering) {
        await signUpWithEmail(window.fetch, cloudApiOrigin(), { name, email, password });
      } else {
        await signInWithEmail(window.fetch, cloudApiOrigin(), { email, password });
      }
      window.location.assign(callbackURL);
    } catch (cause) {
      setError(cause instanceof AuthenticationError ? cause.message : "Authentication failed.");
    } finally {
      setBusy(false);
    }
  }

  return (
    <main>
      <h1>{registering ? "Create Owner account" : "Sign in"}</h1>
      <form onSubmit={(event) => void submit(event)}>
        {registering ? <label>Name<input name="name" required autoComplete="name" /></label> : null}
        <label>Email<input name="email" type="email" required autoComplete="email" /></label>
        <label>Password<input name="password" type="password" required autoComplete={registering ? "new-password" : "current-password"} /></label>
        {error !== null ? <p role="alert">{error}</p> : null}
        <button type="submit" disabled={busy}>{busy ? "Working…" : registering ? "Create account" : "Sign in"}</button>
      </form>
      <button type="button" disabled={busy} onClick={() => setRegistering(!registering)}>
        {registering ? "I already have an account" : "Create account"}
      </button>
    </main>
  );
}
