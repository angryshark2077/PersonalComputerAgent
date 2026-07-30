import { Hono } from "hono";

const app = new Hono();

app.get("/health", (c) =>
  c.json({
    ready: true,
    service: "pca-cloud-api-scaffold",
  }),
);

export default app;
