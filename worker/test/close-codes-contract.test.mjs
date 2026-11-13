import test from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { shouldReconnectAfterClose } from "../public/recovery.js";

// fixtures/close-codes-contract.json: what the browser does on each close code of its signaling socket.
const contract = JSON.parse(readFileSync(new URL("../../fixtures/close-codes-contract.json", import.meta.url), "utf8"));
const POLICY_CODE = 1008;

test("browser retries every relay close code except the policy close", () => {
  const codes = new Set([...contract.closes.filter((row) => row.sentBy === "relay").map((row) => row.code),
    ...contract.clientActions.otherCodes.codes]);
  assert.ok(codes.has(POLICY_CODE));
  for (const code of codes) {
    const actions = code === POLICY_CODE ? contract.clientActions.policy1008 : contract.clientActions.otherCodes;
    assert.equal(shouldReconnectAfterClose(code, false), actions.browser.signaling === "retry", `close ${code}`);
  }
});
