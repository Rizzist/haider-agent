import assert from "node:assert/strict";
import test from "node:test";
import { BrowserPresence, detachMeansStop, GROUP_COLOR, GROUP_TITLE } from "./presence.js";

test("the infobar Cancel is Stop; closing or releasing a tab is not", () => {
  assert.equal(detachMeansStop("canceled_by_user"), true);
  assert.equal(detachMeansStop("target_closed"), false);
  assert.equal(detachMeansStop("released"), false);
});

test("first attach shows presence, Stop is reported once, last detach hides", () => {
  const presence = new BrowserPresence();
  assert.deepEqual(presence.attached(1), [{ event: "presence", surface: "browser", state: "shown" }]);
  assert.deepEqual(presence.attached(2), []);
  assert.deepEqual(presence.detached(1, "canceled_by_user"), [
    { event: "stop", surface: "browser", tabId: 1, reason: "canceled_by_user" },
  ]);
  assert.deepEqual(presence.detached(2, "canceled_by_user"), [
    { event: "presence", surface: "browser", state: "hidden", reason: "canceled_by_user" },
  ]);
  assert.deepEqual(presence.detached(3, "released"), [], "unknown tabs are ignored");
});

test("agent tabs are grouped under an orange Haider label", () => {
  assert.equal(GROUP_TITLE, "Haider");
  assert.equal(GROUP_COLOR, "orange");
});
