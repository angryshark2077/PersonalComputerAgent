import assert from "node:assert/strict";
import test from "node:test";

import type {
  RecoverCommunicationObjectInput,
  UnlinkedCommunicationAttachment,
} from "@pca/db-cloud/src/repository.js";

import { runCommunicationObjectRecovery } from "../communication-object-recovery.js";

const attachment: UnlinkedCommunicationAttachment = {
  workspaceId: "01982222-7222-8222-8222-222222222222",
  deviceId: "01981111-7111-8111-8111-111111111111",
  eventId: "01986666-7666-8666-8666-666666666667",
  attachmentId: "wechat-video:one",
  sha256: "a".repeat(64),
  sizeBytes: 4096,
  mimeType: "video/mp4",
};

test("recovery relinks only an unindexed R2 object with an exact media manifest", async () => {
  const recovered: RecoverCommunicationObjectInput[] = [];
  const result = await runCommunicationObjectRecovery({
    listUnlinkedCommunicationAttachments: async () => [attachment],
    listCommunicationObjectKeys: async () => ["communication/already-indexed"],
    recoverCompletedCommunicationObject: async (input) => {
      recovered.push(input);
      return true;
    },
  }, {
    listObjects: async (prefix) => {
      assert.equal(prefix, "communication/");
      return [
        {
          objectKey: "communication/already-indexed",
          sha256: attachment.sha256,
          sizeBytes: attachment.sizeBytes,
          mimeType: attachment.mimeType,
        },
        {
          objectKey: "communication/wrong-size",
          sha256: attachment.sha256,
          sizeBytes: attachment.sizeBytes + 1,
          mimeType: attachment.mimeType,
        },
        {
          objectKey: "communication/orphan",
          sha256: attachment.sha256,
          sizeBytes: attachment.sizeBytes,
          mimeType: attachment.mimeType,
        },
      ];
    },
  }, new Date("2026-08-05T08:00:00Z"));

  assert.deepEqual(result, { missing: 1, orphanObjects: 2, recovered: 1, unmatched: 0 });
  assert.equal(recovered.length, 1);
  assert.equal(recovered[0]?.objectKey, "communication/orphan");
  assert.equal(recovered[0]?.eventId, attachment.eventId);
  assert.equal(recovered[0]?.now.toISOString(), "2026-08-05T08:00:00.000Z");
});

test("recovery leaves unmatched attachments unchanged", async () => {
  let writes = 0;
  const result = await runCommunicationObjectRecovery({
    listUnlinkedCommunicationAttachments: async () => [attachment],
    listCommunicationObjectKeys: async () => [],
    recoverCompletedCommunicationObject: async () => {
      writes += 1;
      return true;
    },
  }, {
    listObjects: async () => [],
  }, new Date("2026-08-05T08:00:00Z"));

  assert.deepEqual(result, { missing: 1, orphanObjects: 0, recovered: 0, unmatched: 1 });
  assert.equal(writes, 0);
});
