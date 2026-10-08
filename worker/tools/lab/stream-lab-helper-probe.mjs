#!/usr/bin/env node

import { randomUUID } from "node:crypto";
import { openDemandStartedHelperSession, validateRunId } from "./stream-lab-runtime.mjs";

const runId = randomUUID();
validateRunId(runId);
let session;

try {
  session = await openDemandStartedHelperSession({
    runId,
    leaseSeconds: 60,
    vdd: false,
  });
  const ended = await session.end(runId);
  session = undefined;
  process.stdout.write(`${JSON.stringify({ ok: true, runId, vdd: false, ended })}\n`);
} catch (error) {
  if (session) {
    try { await session.end(runId); } catch { session.close(); }
  }
  process.stderr.write(`${JSON.stringify({ ok: false, runId, error: error?.message ?? String(error) })}\n`);
  process.exitCode = 1;
}
