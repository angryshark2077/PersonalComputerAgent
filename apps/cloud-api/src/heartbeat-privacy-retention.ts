import type { ControlRepository } from "@pca/db-cloud/src/repository.js";

export const heartbeatPrivacyRetentionMilliseconds = 30 * 24 * 60 * 60 * 1_000;
export const heartbeatPrivacyRetentionIntervalMilliseconds = 15 * 60 * 1_000;
const retentionBatchSize = 1_000;
const maximumBatchesPerPass = 10;

type HeartbeatPrivacyRetentionRepository = Pick<
  ControlRepository,
  "redactExpiredHeartbeatNetworkData"
>;

export async function runHeartbeatPrivacyRetentionPass(
  repository: HeartbeatPrivacyRetentionRepository,
  now: Date,
): Promise<number> {
  const capturedBefore = new Date(now.getTime() - heartbeatPrivacyRetentionMilliseconds);
  let redacted = 0;
  for (let batch = 0; batch < maximumBatchesPerPass; batch += 1) {
    const count = await repository.redactExpiredHeartbeatNetworkData(
      capturedBefore,
      retentionBatchSize,
    );
    redacted += count;
    if (count < retentionBatchSize) break;
  }
  return redacted;
}

export function startHeartbeatPrivacyRetentionWorker(
  repository: HeartbeatPrivacyRetentionRepository,
): { stop(): void } {
  let running = false;
  const execute = async () => {
    if (running) return;
    running = true;
    try {
      await runHeartbeatPrivacyRetentionPass(repository, new Date());
    } catch {
      console.error("heartbeat privacy retention pass failed");
    } finally {
      running = false;
    }
  };

  void execute();
  const timer = setInterval(() => void execute(), heartbeatPrivacyRetentionIntervalMilliseconds);
  timer.unref();
  return { stop: () => clearInterval(timer) };
}
