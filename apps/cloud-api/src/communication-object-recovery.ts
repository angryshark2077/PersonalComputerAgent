import { randomUUID } from "node:crypto";

import type {
  ControlRepository,
  RecoverCommunicationObjectInput,
  UnlinkedCommunicationAttachment,
} from "@pca/db-cloud/src/repository.js";

import type { R2ObjectStore, R2StoredObject } from "./r2.js";

const recoveryLimit = 10_000;

type RecoveryRepository = Pick<
  ControlRepository,
  | "listUnlinkedCommunicationAttachments"
  | "listCommunicationObjectKeys"
  | "recoverCompletedCommunicationObject"
>;

export interface CommunicationObjectRecoveryResult {
  missing: number;
  orphanObjects: number;
  recovered: number;
  unmatched: number;
}

export async function runCommunicationObjectRecovery(
  repository: RecoveryRepository,
  objectStore: Pick<R2ObjectStore, "listObjects">,
  now: Date,
): Promise<CommunicationObjectRecoveryResult> {
  const missing = await repository.listUnlinkedCommunicationAttachments(recoveryLimit);
  if (missing.length === 0) return { missing: 0, orphanObjects: 0, recovered: 0, unmatched: 0 };

  const knownKeys = new Set(await repository.listCommunicationObjectKeys());
  const orphanObjects = (await objectStore.listObjects("communication/"))
    .filter((object) => !knownKeys.has(object.objectKey));
  const candidates = new Map<string, R2StoredObject[]>();
  for (const object of orphanObjects) {
    const key = recoveryKey(object.sha256, object.sizeBytes, object.mimeType);
    const bucket = candidates.get(key) ?? [];
    bucket.push(object);
    candidates.set(key, bucket);
  }

  let recovered = 0;
  for (const attachment of missing) {
    const bucket = candidates.get(recoveryKey(
      attachment.sha256,
      attachment.sizeBytes,
      attachment.mimeType,
    ));
    const object = bucket?.shift();
    if (object === undefined) continue;
    if (await repository.recoverCompletedCommunicationObject(recoveryInput(attachment, object, now))) {
      recovered += 1;
    }
  }
  return {
    missing: missing.length,
    orphanObjects: orphanObjects.length,
    recovered,
    unmatched: missing.length - recovered,
  };
}

export function startCommunicationObjectRecovery(
  repository: RecoveryRepository,
  objectStore: Pick<R2ObjectStore, "listObjects">,
): void {
  void runCommunicationObjectRecovery(repository, objectStore, new Date())
    .then((result) => {
      if (result.missing > 0) {
        console.log(
          `communication object recovery found ${result.missing} missing, restored ${result.recovered}, left ${result.unmatched} unmatched`,
        );
      }
    })
    .catch(() => console.error("communication object recovery pass failed"));
}

function recoveryInput(
  attachment: UnlinkedCommunicationAttachment,
  object: R2StoredObject,
  now: Date,
): RecoverCommunicationObjectInput {
  return {
    ...attachment,
    objectId: randomUUID(),
    objectKey: object.objectKey,
    now,
  };
}

function recoveryKey(sha256: string, sizeBytes: number, mimeType: string): string {
  return `${sha256}:${sizeBytes}:${mimeType}`;
}
