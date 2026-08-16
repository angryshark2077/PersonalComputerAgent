import assert from "node:assert/strict";
import test from "node:test";

import {
  cloudSchema,
  deviceScreenshotRequests,
  deviceScreenshots,
} from "../src/schema.js";

test("cloud schema registers both screenshot tables", () => {
  assert.equal(cloudSchema.deviceScreenshotRequests, deviceScreenshotRequests);
  assert.equal(cloudSchema.deviceScreenshots, deviceScreenshots);
});
