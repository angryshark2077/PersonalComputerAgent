import { serve, type ServerType } from "@hono/node-server";
import { MemoryControlRepository } from "@pca/db-cloud/src/repository.js";
import { randomBytes } from "node:crypto";

import { createApp, type OwnerPrincipal } from "../../index.js";
import { pkceChallenge } from "../../pairing.js";

const owner: OwnerPrincipal = {
  userId: "01981111-7111-8111-8111-111111111111",
  workspaceId: "01982222-7222-8222-8222-222222222222",
};

export interface S1bPairingHandoff {
  sessionId: string;
  callbackState: string;
}

export interface S1bExchangedDevice {
  deviceId: string;
  workspaceId: string;
}

export interface S1bAcceptanceInspection {
  exchangeCount: number;
  controlRequests: Array<{ deviceId: string | null; status: number }>;
  nonCredentialJson: readonly Buffer[];
  pkce: {
    pairingStarts: number;
    verifierDiffersFromCallbackState: boolean;
    challengeMatched: boolean;
  };
  sensitiveValues: readonly string[];
}

export interface S1bAcceptanceCloud {
  origin: string;
  owner: OwnerPrincipal;
  waitForPairingStart(): Promise<S1bPairingHandoff>;
  acceptCallback(redirect: string, callbackState: string): Promise<string>;
  waitForExchange(): Promise<S1bExchangedDevice>;
  inspect(): S1bAcceptanceInspection;
  close(): Promise<void>;
}

interface CredentialGrant {
  workspace_id: string;
  device_id: string;
  device_access_token: string;
  refresh_token: string;
}

interface PairingStartRequest {
  callback_state: string;
  code_challenge: string;
}

interface PairingExchangeRequest {
  code_verifier: string;
}

export async function createS1bAcceptanceCloud(): Promise<S1bAcceptanceCloud> {
  const repository = new MemoryControlRepository([owner]);
  const app = createApp({
    repository,
    ownerAuthenticator: async () => owner,
    clientAddress: () => "203.0.113.10",
  });
  const callbackCodes = new Map<string, string>();
  const credentialByAccessToken = new Map<string, S1bExchangedDevice>();
  const controlRequests: Array<{ deviceId: string | null; status: number }> = [];
  const nonCredentialJson: Buffer[] = [];
  const messageCanary = `accept-canary-${randomBytes(24).toString("base64url")}`;
  const sensitiveValues = new Set<string>([messageCanary]);
  let pairingStarts = 0;
  let pairingCallbackState: string | null = null;
  let pairingCodeChallenge: string | null = null;
  let verifierDiffersFromCallbackState = false;
  let challengeMatched = false;
  let resolvePairingStart: (handoff: S1bPairingHandoff) => void;
  let rejectPairingStart: (error: Error) => void;
  const pairingStarted = new Promise<S1bPairingHandoff>((resolve, reject) => {
    resolvePairingStart = resolve;
    rejectPairingStart = reject;
  });
  let exchangeCount = 0;
  let resolveExchange: (device: S1bExchangedDevice) => void;
  let rejectExchange: (error: Error) => void;
  const exchanged = new Promise<S1bExchangedDevice>((resolve, reject) => {
    resolveExchange = resolve;
    rejectExchange = reject;
  });
  let releaseConfiguredControl: () => void;
  const configured = new Promise<void>((resolve) => {
    releaseConfiguredControl = resolve;
  });

  app.get("/pca/pair/callback", (context) => {
    const code = context.req.query("code");
    const state = context.req.query("state");
    if (code === undefined || state === undefined || callbackCodes.has(state)) {
      return context.text("Invalid pairing callback.", 400);
    }
    callbackCodes.set(state, code);
    sensitiveValues.add(code);
    return context.text("Pairing callback accepted.");
  });
  app.get("/test/s1b/non-credential-canary", (context) =>
    context.json({ message_canary: messageCanary })
  );

  let origin = "";
  const server = await new Promise<ServerType>((resolve, reject) => {
    const candidate = serve(
      {
        hostname: "127.0.0.1",
        port: 0,
        fetch: async (request) => {
          const url = new URL(request.url);
          if (url.pathname === "/v1/agent/control") await configured;
          const pairingStart = url.pathname === "/v1/device-pairing/sessions"
            && request.method === "POST"
            ? await request.clone().json().catch(() => null) as PairingStartRequest | null
            : null;
          const pairingExchange = url.pathname === "/v1/device-pairing/exchange"
            && request.method === "POST"
            ? await request.clone().json().catch(() => null) as PairingExchangeRequest | null
            : null;
          const response = await app.fetch(request);
          const contentType = response.headers.get("content-type") ?? "";

          if (pairingStart !== null) {
            if (response.status === 201
              && typeof pairingStart.callback_state === "string"
              && typeof pairingStart.code_challenge === "string") {
              try {
                const body = (await response.clone().json()) as { session_id: string };
                pairingStarts += 1;
                pairingCallbackState = pairingStart.callback_state;
                pairingCodeChallenge = pairingStart.code_challenge;
                sensitiveValues.add(pairingStart.callback_state);
                resolvePairingStart({
                  sessionId: body.session_id,
                  callbackState: pairingStart.callback_state,
                });
              } catch (error) {
                rejectPairingStart(
                  error instanceof Error ? error : new Error("invalid pairing session response"),
                );
              }
            } else if (!response.ok) {
              rejectPairingStart(new Error(`pairing start returned ${response.status}`));
            }
          }
          if (pairingExchange !== null && pairingCallbackState !== null && pairingCodeChallenge !== null) {
            verifierDiffersFromCallbackState =
              pairingExchange.code_verifier !== pairingCallbackState;
            challengeMatched =
              pkceChallenge(pairingExchange.code_verifier) === pairingCodeChallenge;
          }
          if (url.pathname === "/v1/device-pairing/exchange" && response.ok) {
            try {
              const grant = (await response.clone().json()) as CredentialGrant;
              const device = { deviceId: grant.device_id, workspaceId: grant.workspace_id };
              credentialByAccessToken.set(grant.device_access_token, device);
              sensitiveValues.add(grant.device_access_token);
              sensitiveValues.add(grant.refresh_token);
              exchangeCount += 1;
              resolveExchange(device);
            } catch (error) {
              rejectExchange(error instanceof Error ? error : new Error("invalid exchange grant"));
            }
          } else if (contentType.includes("application/json")) {
            nonCredentialJson.push(Buffer.from(await response.clone().arrayBuffer()));
          }

          if (url.pathname.includes("/collector-config") && request.method === "PUT" && response.ok) {
            releaseConfiguredControl();
          }
          if (url.pathname === "/v1/agent/control") {
            const token = bearerToken(request.headers.get("authorization"));
            controlRequests.push({
              deviceId: token === null ? null : credentialByAccessToken.get(token)?.deviceId ?? null,
              status: response.status,
            });
          }
          return response;
        },
      },
      (address) => {
        origin = `http://127.0.0.1:${address.port}`;
        resolve(candidate);
      },
    );
    candidate.once("error", reject);
  });

  return {
    origin,
    owner,
    waitForPairingStart: () => pairingStarted,
    async acceptCallback(redirect, callbackState) {
      const response = await fetch(redirect);
      if (!response.ok) throw new Error(`pairing callback returned ${response.status}`);
      const callbackCode = callbackCodes.get(callbackState);
      if (callbackCode === undefined) throw new Error("pairing callback did not deliver a code");
      return callbackCode;
    },
    waitForExchange: () => exchanged,
    inspect: () => ({
      exchangeCount,
      controlRequests: controlRequests.map((request) => ({ ...request })),
      nonCredentialJson: nonCredentialJson.map((value) => Buffer.from(value)),
      pkce: {
        pairingStarts,
        verifierDiffersFromCallbackState,
        challengeMatched,
      },
      sensitiveValues: [...sensitiveValues],
    }),
    close: () => new Promise((resolve, reject) => {
      server.close((error) => error === undefined ? resolve() : reject(error));
    }),
  };
}

function bearerToken(header: string | null): string | null {
  return header?.startsWith("Bearer ") === true ? header.slice("Bearer ".length) : null;
}
