import type { DashboardSystemMetric } from "./api";

export interface SystemMetricsSummary {
  cpu: string | null;
  memory: string | null;
  disk: string | null;
}

export function summarizeSystemMetrics(metrics: DashboardSystemMetric[]): SystemMetricsSummary {
  const cpuMemory = metrics.find((metric) => metric.metric_group === "cpu_memory");
  const disk = metrics.find((metric) => metric.metric_group === "disk");
  const host = cpuMemory === undefined ? null : record(cpuMemory.payload.host);
  const cpuUsage = host === null ? null : number(host.cpu_usage_percent);
  const memoryTotal = host === null ? null : number(host.memory_total_bytes);
  const memoryUsed = host === null ? null : number(host.memory_used_bytes);
  const diskTotal = disk === undefined ? null : number(disk.payload.total_bytes);
  const diskAvailable = disk === undefined ? null : number(disk.payload.available_bytes);
  const diskUsedPercent = disk === undefined ? null : number(disk.payload.used_percent);

  return {
    cpu: cpuUsage === null ? null : `${cpuUsage.toFixed(1)}%`,
    memory: memoryTotal === null || memoryUsed === null
      ? null
      : `${gibibytes(memoryUsed)} / ${gibibytes(memoryTotal)}`,
    disk: diskTotal === null || diskAvailable === null || diskUsedPercent === null
      ? null
      : `${gibibytes(diskAvailable)} available of ${gibibytes(diskTotal)} (${diskUsedPercent.toFixed(1)}% used)`,
  };
}

function record(value: unknown): Record<string, unknown> | null {
  return typeof value === "object" && value !== null && !Array.isArray(value)
    ? value as Record<string, unknown>
    : null;
}

function number(value: unknown): number | null {
  return typeof value === "number" && Number.isFinite(value) && value >= 0 ? value : null;
}

function gibibytes(bytes: number): string {
  return `${(bytes / (1024 ** 3)).toFixed(1)} GiB`;
}
