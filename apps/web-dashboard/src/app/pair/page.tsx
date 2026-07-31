"use client";

import { useSearchParams } from "next/navigation";
import { Suspense, useEffect, useState } from "react";

import { DashboardApiError, authorizePairing, cloudApiOrigin } from "../../lib/api";
import { getBrowserSession, redirectToSignIn } from "../../lib/auth";
import { parsePairingHandoff } from "../../lib/pairing-state";

export default function PairPage() {
  return (
    <Suspense fallback={<main><p>Loading pairing…</p></main>}>
      <PairScreen />
    </Suspense>
  );
}

function PairScreen() {
  const params = useSearchParams();
  const handoff = parsePairingHandoff(params);
  const [authorized, setAuthorized] = useState(false);
  const [sessionReady, setSessionReady] = useState(false);
  const [error, setError] = useState<string | null>(handoff === null ? "Pairing session is invalid or expired." : null);

  useEffect(() => {
    void (async () => {
      if ((await getBrowserSession(window.fetch, cloudApiOrigin())) === null) {
        redirectToSignIn();
        return;
      }
      setSessionReady(true);
    })();
  }, []);

  async function authorize(): Promise<void> {
    if (handoff === null) return;
    setAuthorized(true);
    setError(null);
    try {
      const redirect = await authorizePairing(
        window.fetch,
        cloudApiOrigin(),
        handoff.sessionId,
        handoff.callbackState,
      );
      window.location.assign(redirect);
    } catch (cause) {
      setAuthorized(false);
      setError(cause instanceof DashboardApiError ? cause.message : "Unable to authorize pairing.");
    }
  }

  return (
    <main>
      <h1>Pair this Mac</h1>
      <p>Your sole Owner Workspace will be bound to this device.</p>
      {error !== null ? <p role="alert">{error}</p> : null}
      <button type="button" disabled={authorized || !sessionReady || handoff === null} onClick={() => void authorize()}>
        {authorized ? "Authorizing…" : "Authorize this Mac"}
      </button>
    </main>
  );
}
