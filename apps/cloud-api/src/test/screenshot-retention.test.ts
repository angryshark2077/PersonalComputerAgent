import assert from "node:assert/strict";
import test from "node:test";

import type { ScreenshotRecord } from "@pca/db-cloud/src/repository.js";

import {
  runScreenshotRetentionPass,
  screenshotRetentionMilliseconds,
} from "../screenshot-retention.js";

const now = new Date("2026-08-08T12:00:00.000Z");

function screenshot(screenshotId: string, capturedAt: Date): ScreenshotRecord {
  return {
    screenshotId,
    workspaceId: "01982222-7222-8222-8222-222222222222",
    deviceId: "01981111-7111-8111-8111-111111111111",
    requestId: null,
    trigger: "activity",
    capturedAt,
    appBundleId: "com.example.App",
    pixelWidth: 1920,
    pixelHeight: 1080,
    objectKey: `screenshots/${screenshotId}`,
    expectedSha256: "a".repeat(64),
    expectedSizeBytes: 1024,
    expectedMimeType: "image/jpeg",
    state: "completed",
    preparedAt: capturedAt,
    completedAt: capturedAt,
  };
}

test("retention deletes an R2 object before its expired database record", async () => {
  const expired = screenshot(
    "01981111-7111-8111-8111-111111111112",
    new Date(now.getTime() - screenshotRetentionMilliseconds - 1),
  );
  const calls: string[] = [];
  const result = await runScreenshotRetentionPass(
    {
      listExpiredCompletedScreenshots: async () => [expired],
      deleteExpiredCompletedScreenshot: async (screenshotId) => {
        calls.push(`database:${screenshotId}`);
        return true;
      },
    },
    {
      deleteObject: async (objectKey) => {
        calls.push(`r2:${objectKey}`);
      },
    },
    now,
  );

  assert.deepEqual(result, { deleted: 1, failed: 0 });
  assert.deepEqual(calls, [
    `r2:${expired.objectKey}`,
    `database:${expired.screenshotId}`,
  ]);
});

test("retention keeps the database record when R2 deletion fails", async () => {
  const expired = screenshot(
    "01981111-7111-8111-8111-111111111113",
    new Date(now.getTime() - screenshotRetentionMilliseconds - 1),
  );
  let databaseDeletionCalled = false;
  const result = await runScreenshotRetentionPass(
    {
      listExpiredCompletedScreenshots: async () => [expired],
      deleteExpiredCompletedScreenshot: async () => {
        databaseDeletionCalled = true;
        return true;
      },
    },
    { deleteObject: async () => { throw new Error("unavailable"); } },
    now,
  );

  assert.deepEqual(result, { deleted: 0, failed: 1 });
  assert.equal(databaseDeletionCalled, false);
});
