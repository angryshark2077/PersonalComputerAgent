import assert from "node:assert/strict";
import test from "node:test";

import { createR2ObjectStore } from "../r2.js";

test("R2 storage is optional only when every R2 variable is absent", () => {
  assert.equal(createR2ObjectStore({}), undefined);
  assert.throws(
    () => createR2ObjectStore({ R2_ENDPOINT: "https://storage.example" }),
    /R2_BUCKET_PUBLIC must be false/,
  );
});

test("R2 configuration rejects non-private or insecure storage", () => {
  assert.throws(
    () => createR2ObjectStore({
      R2_ENDPOINT: "http://storage.example",
      R2_ACCESS_KEY_ID: "key",
      R2_SECRET_ACCESS_KEY: "secret",
      R2_BUCKET: "media",
      R2_BUCKET_PUBLIC: "false",
    }),
    /HTTPS/,
  );
  assert.throws(
    () => createR2ObjectStore({
      R2_ENDPOINT: "https://storage.example",
      R2_ACCESS_KEY_ID: "key",
      R2_SECRET_ACCESS_KEY: "secret",
      R2_BUCKET: "media",
      R2_BUCKET_PUBLIC: "true",
    }),
    /must be false/,
  );
});
