import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import path from "node:path";
import test from "node:test";
import { fileURLToPath } from "node:url";

const here = path.dirname(fileURLToPath(import.meta.url));
const driver = path.join(here, "profile-scrape.mjs");

test("profile scrape rejects a non-positive profile count before connecting", () => {
  const result = spawnSync(process.execPath, [driver], {
    encoding: "utf8",
    env: {
      ...process.env,
      PROFILE_COUNT: "0",
      HOST: "127.0.0.1:1",
    },
  });

  assert.equal(result.status, 1);
  assert.match(result.stderr, /PROFILE_COUNT must be a positive integer/);
});
