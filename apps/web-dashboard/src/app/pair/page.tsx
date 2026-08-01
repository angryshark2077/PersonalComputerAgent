"use client";

import { useSearchParams } from "next/navigation";
import { Suspense, useEffect, useState } from "react";

import { cloudApiOrigin, pairingAuthorizePath } from "../../lib/api";
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

  function authorize() {
    setAuthorized(true);
  }

  return (
    <main>
      <h1>Pair this Mac</h1>
      <p>Your sole Owner Workspace will be bound to this device.</p>
      {error !== null ? <p role="alert">{error}</p> : null}
      {handoff !== null ? (
        <form method="post" action={pairingAuthorizePath(handoff.sessionId)} onSubmit={authorize}>
          <input type="hidden" name="callback_state" value={handoff.callbackState} />
          <button type="submit" disabled={authorized || !sessionReady}>
            {authorized ? "Authorizing…" : "Authorize this Mac"}
          </button>
        </form>
      ) : null}
    </main>
  );
}
