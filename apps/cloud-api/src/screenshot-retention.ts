import type { ControlRepository } from "@pca/db-cloud/src/repository.js";

import type { R2ObjectStore } from "./r2.js";

export const screenshotRetentionMilliseconds = 7 * 24 * 60 * 60 * 1_000;
export const screenshotRetentionIntervalMilliseconds = 15 * 60 * 1_000;
const retentionBatchSize = 100;
const maximumBatchesPerPass = 10;

type ScreenshotRetentionRepository = Pick<
  ControlRepository,
  "listExpiredCompletedScreenshots" | "deleteExpiredCompletedScreenshot"
>;

export interface ScreenshotRetentionResult {
  deleted: number;
  failed: number;
}

export async function runScreenshotRetentionPass(
  repository: ScreenshotRetentionRepository,
  objectStore: Pick<R2ObjectStore, "deleteObject">,
  now: Date,
): Promise<ScreenshotRetentionResult> {
  const capturedBefore = new Date(now.getTime() - screenshotRetentionMilliseconds);
  let deleted = 0;
  let failed = 0;

  for (let batch = 0; batch < maximumBatchesPerPass; batch += 1) {
    const screenshots = await repository.listExpiredCompletedScreenshots(
      capturedBefore,
      retentionBatchSize,
    );
    if (screenshots.length === 0) break;

    let removedFromBatch = 0;
    for (const screenshot of screenshots) {
      try {
        await objectStore.deleteObject(screenshot.objectKey);
        if (await repository.deleteExpiredCompletedScreenshot(
          screenshot.screenshotId,
          capturedBefore,
        )) {
          deleted += 1;
          removedFromBatch += 1;
        }
      } catch {
        failed += 1;
      }
    }
    if (screenshots.length < retentionBatchSize || removedFromBatch === 0) break;
  }

  return { deleted, failed };
}

export function startScreenshotRetentionWorker(
  repository: ScreenshotRetentionRepository,
  objectStore: Pick<R2ObjectStore, "deleteObject">,
): { stop(): void } {
  let running = false;
  const execute = async () => {
    if (running) return;
    running = true;
    try {
      const result = await runScreenshotRetentionPass(repository, objectStore, new Date());
      if (result.failed > 0) {
        console.error(`screenshot retention completed with ${result.failed} failed deletion(s)`);
      }
    } catch {
      console.error("screenshot retention pass failed");
    } finally {
      running = false;
    }
  };

  void execute();
  const timer = setInterval(() => void execute(), screenshotRetentionIntervalMilliseconds);
  timer.unref();
  return { stop: () => clearInterval(timer) };
}
