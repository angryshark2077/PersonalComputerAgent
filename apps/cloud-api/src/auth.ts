import type { Context } from "hono";

import {
  ControlRepositoryError,
  type ControlRepository,
} from "@pca/db-cloud/src/repository.js";

import { errorResponse, hashSecret } from "./pairing.js";

export interface OwnerPrincipal {
  userId: string;
  workspaceId: string;
}

export type OwnerAuthenticator = (request: Request) => Promise<OwnerPrincipal | null>;

export interface BetterAuthSessionReader {
  api: {
    getSession(input: { headers: Headers }): Promise<{ user?: { id?: string } } | null>;
  };
}

export function createBetterAuthOwnerAuthenticator(
  auth: BetterAuthSessionReader,
  repository: ControlRepository,
): OwnerAuthenticator {
  return async (request) => {
    const session = await auth.api.getSession({ headers: request.headers });
    const userId = session?.user?.id;
    if (userId === undefined) {
      return null;
    }
    const workspaceId = await repository.resolveOwnerWorkspace(userId);
    return workspaceId === null ? null : { userId, workspaceId };
  };
}

export async function requireOwner(
  context: Context,
  ownerAuthenticator: OwnerAuthenticator | undefined,
): Promise<OwnerPrincipal | Response> {
  const principal = await ownerAuthenticator?.(context.req.raw);
  return principal ?? errorResponse(context, 401, "AUTH_REQUIRED", "Owner authentication is required.");
}

export async function requireDevice(
  context: Context,
  repository: ControlRepository,
  kind: "access" | "refresh",
): Promise<{ workspaceId: string; deviceId: string } | Response> {
  const authorization = context.req.header("authorization");
  const token = authorization?.match(/^Bearer ([A-Za-z0-9_-]+)$/)?.[1];
  if (token === undefined) {
    return errorResponse(context, 401, "AUTH_REQUIRED", "Device credentials are required.");
  }
  try {
    const tokenHash = hashSecret(token);
    return kind === "access"
      ? await repository.authenticateDeviceAccess(tokenHash, new Date())
      : await repository.authenticateDeviceRefresh(tokenHash, new Date());
  } catch (error) {
    return repositoryErrorResponse(context, error);
  }
}

export function repositoryErrorResponse(context: Context, error: unknown): Response {
  if (!(error instanceof ControlRepositoryError)) {
    throw error;
  }
  const status =
    error.code === "WORKSPACE_FORBIDDEN"
      ? 403
      : error.code === "DEVICE_NOT_FOUND"
        ? 404
      : error.code === "PAIRING_EXPIRED"
        ? 410
        : error.code === "PAIRING_REPLAYED" || error.code === "CONFLICT"
          ? 409
          : error.code === "PKCE_INVALID"
            ? 400
            : 401;
  return errorResponse(context, status, error.code, messageFor(error.code));
}

function messageFor(code: ControlRepositoryError["code"]): string {
  switch (code) {
    case "DEVICE_REVOKED":
      return "The device has been revoked.";
    case "PAIRING_EXPIRED":
      return "The pairing session has expired.";
    case "PAIRING_REPLAYED":
      return "The pairing code has already been used.";
    case "PKCE_INVALID":
      return "The PKCE verifier is invalid.";
    case "WORKSPACE_FORBIDDEN":
      return "The requested Workspace is forbidden.";
    default:
      return "The device credential is invalid.";
  }
}
