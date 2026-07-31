export interface PairingHandoff {
  sessionId: string;
  callbackState: string;
}

export function parsePairingHandoff(params: URLSearchParams): PairingHandoff | null {
  const sessionId = params.get("session_id");
  const callbackState = params.get("callback_state");
  if (
    sessionId === null ||
    sessionId.length === 0 ||
    callbackState === null ||
    !/^[A-Za-z0-9_-]{43,}$/.test(callbackState)
  ) {
    return null;
  }
  return { sessionId, callbackState };
}
