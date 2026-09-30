import test from "node:test";
import assert from "node:assert/strict";
import { existsSync, mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { spawnSync } from "node:child_process";
import { createServer } from "node:net";

import { STREAM_LAB_COMPATIBILITY } from "../public/media.js";

import {
  STREAM_LAB_PIPE,
  buildEncoderRound,
  buildHelperRequest,
  cleanLocalExpiredRuns,
  encoderCampaignPlan,
  failedOptimizationState,
  optimizationCampaignPlan,
  persistentReceiverProfile,
  prepareLabExecution,
  runPreflight,
  buildCandidateHost,
  buildManualCandidate,
  buildScreeningCandidate,
  reportExitCode,
  parseDeltaTriggerFrames,
  parseSceneOptions,
  parseWorkerVersionOptions,
  isLoopbackLabUrl,
  performanceActionProbeEnabled,
  receiverAssetInterceptionForRun,
  sendHelperRequest,
  validateLiveRunRouting,
} from "../tools/stream-lab.mjs";

test("performance input probing can be disabled without changing the media candidate", () => {
  assert.equal(performanceActionProbeEnabled([], false, false), true);
  assert.equal(performanceActionProbeEnabled(["--no-action-probe"], false, false), false);
  assert.equal(performanceActionProbeEnabled([], true, false), false);
  assert.equal(performanceActionProbeEnabled([], false, true), false);
});

test("screening exposes CQ23 external-budget VBR as one default-off axis", () => {
  const candidate = buildScreeningCandidate(["--rate-control", "external-budget-vbr"]);
  assert.equal(candidate.experimentId, "external-budget-vbr-cq23");
  assert.equal(candidate.rateControlMode, "external-budget-vbr");
  assert.equal(candidate.targetQuality, 23);
  assert.equal(candidate.peakBitrateMultiplier, 4);
  assert.equal(candidate.vbvFrames, 4);
  assert.throws(() => buildScreeningCandidate([
    "--rate-control", "external-budget-vbr", "--roi-mode", "feedback",
  ]), /separate hypotheses/i);
});

test("candidate Worker version options are explicit, canonical, and pairwise", () => {
  assert.equal(parseWorkerVersionOptions([], {}), null);
  assert.deepEqual(parseWorkerVersionOptions([
    "--worker-name", "playfull-cat",
    "--worker-version-id", "1a10a2ec-6b6a-4d6a-a7e6-5fd5696360d1",
  ], {}), {
    workerName: "playfull-cat",
    versionId: "1a10a2ec-6b6a-4d6a-a7e6-5fd5696360d1",
  });
  assert.deepEqual(parseWorkerVersionOptions([], {
    BUBBLEPLAY_WORKER_NAME: "playfull-cat",
    BUBBLEPLAY_WORKER_VERSION_ID: "1a10a2ec-6b6a-4d6a-a7e6-5fd5696360d1",
  }), {
    workerName: "playfull-cat",
    versionId: "1a10a2ec-6b6a-4d6a-a7e6-5fd5696360d1",
  });
  assert.throws(() => parseWorkerVersionOptions(["--worker-name", "playfull-cat"], {}), /both/i);
  assert.throws(() => parseWorkerVersionOptions([
    "--worker-name", "Playfull-Cat",
    "--worker-version-id", "not-a-version",
  ], {}), /canonical/i);
});

test("only explicit HTTP loopback URLs qualify for unpinned local relay runs", () => {
  assert.equal(isLoopbackLabUrl("http://127.0.0.1:8787/"), true);
  assert.equal(isLoopbackLabUrl("http://localhost:8787/"), true);
  assert.equal(isLoopbackLabUrl("http://[::1]:8787/"), true);
  assert.equal(isLoopbackLabUrl("https://127.0.0.1:8787/"), false);
  assert.equal(isLoopbackLabUrl("https://playfull-cat.example/"), false);
  assert.equal(isLoopbackLabUrl(undefined), false);
});

test("loopback runs consume receiver assets from the direct relay without CDP interception", () => {
  assert.equal(receiverAssetInterceptionForRun(true), false);
  assert.equal(receiverAssetInterceptionForRun(false), true);
});

test("a pinned remote Worker can use the Lab Helper transient display without weakening routing admission", () => {
  const workerVersionOverride = {
    workerName: "playfull-cat",
    versionId: "a34966a1-d238-4dec-8dbe-5405f6a9fdce",
  };
  assert.deepEqual(validateLiveRunRouting(
    "https://playfull-cat.your-subdomain.workers.dev/",
    workerVersionOverride,
    true,
  ), { loopbackRelay: false, transientVdd: true });
  assert.deepEqual(validateLiveRunRouting("http://127.0.0.1:8787/", null, true),
    { loopbackRelay: true, transientVdd: true });
  assert.throws(() => validateLiveRunRouting("http://127.0.0.1:8787/", workerVersionOverride, true),
    /omit --worker-version-id/);
  assert.throws(() => validateLiveRunRouting("https://play.example/", null, true),
    /require --worker-version-id/);
});

test("delta trigger CLI flags keep matched controls on the candidate source frame", () => {
  assert.equal(parseDeltaTriggerFrames([]), null);
  assert.equal(parseDeltaTriggerFrames(["--delta-trigger-frames", "3"]), 3);
  assert.equal(parseDeltaTriggerFrames(["--delta-trigger-frames", "9"]), 9);
  assert.throws(() => parseDeltaTriggerFrames(["--delta-trigger-frames", "0"]), /trigger/i);
  assert.throws(() => parseDeltaTriggerFrames(["--delta-trigger-frames", "17"]), /trigger/i);
  assert.throws(() => parseDeltaTriggerFrames(["--delta-trigger-frames", "nope"]), /trigger/i);
});

test("quality scene CLI flags select a bounded comma-separated subset", () => {
  assert.deepEqual(parseSceneOptions(["--scenes", "source-code,motion-60"]), ["source-code", "motion-60"]);
  assert.equal(parseSceneOptions([]), null);
  assert.throws(() => parseSceneOptions(["--scenes", "source-code,unknown"]), /scene/i);
  assert.throws(() => parseSceneOptions(["--scenes", ""]), /scene/i);
});

test("persistent-resource smoke profiles change only the NVENC resource axis", () => {
  const candidate = buildScreeningCandidate(["--resource-mode", "persistent"]);
  assert.deepEqual(candidate, {
    id: "nvenc-persistent",
    schemaVersion: 2,
    implementationVariant: "candidate",
    experimentId: "nvenc-persistent",
    preset: "p3",
    tuning: "ultra-low-latency",
    multipass: "disabled",
    aqMode: "spatial",
    aqStrength: 8,
    vbvFrames: 1,
    lookaheadFrames: 0,
    weightedPrediction: false,
    dirtyQpMap: false,
    main10: false,
    roiMode: "off",
    resourceMode: "persistent",
    asyncDepth: 0,
    hybridMode: "off",
    gopSeconds: 2,
    bitrateMbps: 6,
  });
  assert.deepEqual(buildScreeningCandidate([]), {
    id: "quality-latency-control",
    schemaVersion: 2,
    implementationVariant: "production",
    experimentId: "quality-latency-control",
    preset: "p3",
    tuning: "ultra-low-latency",
    multipass: "disabled",
    aqMode: "spatial",
    aqStrength: 8,
    vbvFrames: 1,
    lookaheadFrames: 0,
    weightedPrediction: false,
    dirtyQpMap: false,
    main10: false,
    roiMode: "off",
    resourceMode: "per-frame",
    asyncDepth: 0,
    hybridMode: "off",
    gopSeconds: 2,
    bitrateMbps: 6,
  });
  assert.deepEqual(buildScreeningCandidate(["--resource-mode", "per-frame"]), {
    id: "nvenc-per-frame-control",
    schemaVersion: 2,
    implementationVariant: "production",
    experimentId: "nvenc-per-frame-control",
    preset: "p3",
    tuning: "ultra-low-latency",
    multipass: "disabled",
    aqMode: "spatial",
    aqStrength: 8,
    vbvFrames: 1,
    lookaheadFrames: 0,
    weightedPrediction: false,
    dirtyQpMap: false,
    main10: false,
    roiMode: "off",
    resourceMode: "per-frame",
    asyncDepth: 0,
    hybridMode: "off",
    gopSeconds: 2,
    bitrateMbps: 6,
  });
  assert.throws(() => buildScreeningCandidate(["--resource-mode", "rotating"]), /resource-mode/i);
  assert.throws(() => buildScreeningCandidate(["--resource-mode", "persistent", "--dirty-qp-map"]),
    /separate hypotheses/i);
  assert.throws(() => buildScreeningCandidate(["--resource-mode", "persistent", "--main10"]),
    /separate hypotheses/i);
});

test("quiescent-detail screening selects exactly the default-off hybrid axis", () => {
  const candidate = buildScreeningCandidate(["--hybrid-mode", "png-quiescent-detail"]);
  assert.equal(candidate.id, "hybrid-png-quiescent-detail");
  assert.equal(candidate.experimentId, "hybrid-png-quiescent-detail");
  assert.equal(candidate.hybridMode, "png-quiescent-detail");
  assert.equal(candidate.hybridCacheMode, "suppress-reactivate");
  assert.equal(candidate.roiMode, "off");
  assert.equal(candidate.resourceMode, "per-frame");
  assert.throws(() => buildScreeningCandidate(["--hybrid-mode", "png-future"]), /hybrid-mode/i);
  const control = buildScreeningCandidate([
    "--hybrid-mode", "png-quiescent-detail", "--hybrid-cache-mode", "dda-gated",
  ]);
  assert.equal(control.id, "hybrid-png-quiescent-detail-dda-gated");
  assert.equal(control.experimentId, "hybrid-png-quiescent-detail-dda-gated");
  assert.equal(control.hybridCacheMode, "dda-gated");
  assert.throws(() => buildScreeningCandidate(["--hybrid-cache-mode", "dda-gated"]), /hybrid-mode/i);
  assert.throws(() => buildScreeningCandidate([
    "--hybrid-mode", "png-static-roi", "--hybrid-cache-mode", "dda-gated",
  ]), /quiescent/i);
});

test("HEVC quiescent tile screening selects exactly one default-off experiment axis", () => {
  const candidate = buildScreeningCandidate(["--hybrid-mode", "hevc-quiescent-tiles"]);
  assert.deepEqual(candidate, {
    id: "hybrid-hevc-quiescent-tiles",
    schemaVersion: 2,
    implementationVariant: "candidate",
    experimentId: "hybrid-hevc-quiescent-tiles",
    preset: "p3",
    tuning: "ultra-low-latency",
    multipass: "disabled",
    aqMode: "spatial",
    aqStrength: 8,
    vbvFrames: 1,
    lookaheadFrames: 0,
    weightedPrediction: false,
    gopSeconds: 2,
    bitrateMbps: 6,
    dirtyQpMap: false,
    main10: false,
    roiMode: "off",
    resourceMode: "per-frame",
    asyncDepth: 0,
    hybridMode: "hevc-quiescent-tiles",
    hybridCacheMode: "suppress-reactivate",
  });
});

test("manual use defaults to the latest quiescent-detail candidate and rejects controls", () => {
  const candidate = buildManualCandidate([]);
  assert.equal(candidate.id, "hybrid-png-quiescent-detail");
  assert.equal(candidate.hybridMode, "png-quiescent-detail");
  assert.equal(candidate.implementationVariant, "candidate");
  assert.throws(() => buildManualCandidate(["--resource-mode", "per-frame"]), /candidate experiment/i);
});

test("candidate build targets the uninstalled lab directory and never invokes service installation", () => {
  const calls = [];
  const executable = buildCandidateHost({
    projectRoot: "C:\\repo",
    spawnSync: (command, args, options) => {
      calls.push({ command, args, options });
      return { status: 0, stdout: "", stderr: "" };
    },
    exists: path => path.endsWith("bubbleplay-host.exe") || path.endsWith("bubbleplay_lab_activity_guard.exe"),
  });
  assert.equal(executable, "C:\\repo\\host\\target-candidate\\release\\bubbleplay-host.exe");
  assert.equal(calls.length, 1);
  assert.equal(calls[0].command, "cargo");
  assert.deepEqual(calls[0].args, ["build", "--release", "--bin", "bubbleplay-host", "--bin",
    "bubbleplay_lab_activity_guard", "--target-dir", "target-candidate"]);
  assert.equal(calls[0].options.cwd, "C:\\repo\\host");
  assert.doesNotMatch(JSON.stringify(calls), /update-service|install-service|RunAs|sc\.exe/i);
});

test("an explicit uninstalled candidate host bypasses the occupied default target", async () => {
  const cli = await import("../tools/stream-lab.mjs");
  assert.equal(typeof cli.selectCandidateHostForRun, "function");
  const root = mkdtempSync(join(tmpdir(), "bubbleplay-explicit-candidate-"));
  try {
    const candidate = join(root, "bubbleplay-host.exe");
    writeFileSync(candidate, `candidate:${STREAM_LAB_COMPATIBILITY.id}`);
    const staleCandidate = join(root, "stale-bubbleplay-host.exe");
    writeFileSync(staleCandidate, "candidate:wss-v3-host20-edge12-bench2-stale-v1");
    let builds = 0;
    assert.equal(cli.selectCandidateHostForRun(
      ["--candidate-host", candidate], {}, { candidateHost: null },
      { buildCandidate: () => { builds++; return "wrong"; } },
    ), candidate);
    assert.equal(builds, 0);
    assert.throws(() => cli.selectCandidateHostForRun(
      ["--candidate-host", staleCandidate], {}, { candidateHost: null },
      { buildCandidate: () => { builds++; return "wrong"; } },
    ), error => error?.code === "candidate-host-compatibility-mismatch" &&
      error?.detail?.path === staleCandidate &&
      error?.detail?.expectedCompatibilityId === STREAM_LAB_COMPATIBILITY.id);
    assert.equal(builds, 0);
    assert.throws(() => cli.selectCandidateHostForRun(
      ["--candidate-host", join(root, "missing.exe")], {}, { candidateHost: null },
    ), /candidate host.*does not exist/i);
  } finally {
    rmSync(root, { recursive: true, force: true });
  }
});

test("the optimizer activity guard resolves beside the uninstalled candidate host", async () => {
  const { defaultActivityGuardPath } = await import("../tools/stream-lab-activity.mjs");
  assert.match(defaultActivityGuardPath(), /host[\\/]target-candidate[\\/]release[\\/]bubbleplay_lab_activity_guard\.exe$/);
});

test("optimizer builds isolated candidate artifacts before its strict guard preflight", () => {
  const events = [];
  const prepared = prepareLabExecution("optimize", {}, {
    preflight: options => {
      events.push(options.requireActivityGuard ? "strict-preflight" : "dependency-preflight");
      return { result: "ready", missing: [] };
    },
    buildCandidate: () => {
      events.push("candidate-build");
      return "C:\\repo\\host\\target-candidate\\release\\bubbleplay-host.exe";
    },
  });
  assert.deepEqual(events, ["dependency-preflight", "candidate-build", "strict-preflight"]);
  assert.equal(prepared.preflight.result, "ready");
  assert.match(prepared.candidateHost, /target-candidate/);
});

test("a failed optimization checkpoint cannot remain logically paused", () => {
  const next = failedOptimizationState({ status: "paused", pauseState: {
    paused: true, reason: "physical-user-activity", since: "old", resumeAfter: "later",
  } }, { code: "source-window-placement" }, "2026-08-25T21:00:00.000Z");
  assert.equal(next.status, "failed");
  assert.deepEqual(next.pauseState, { paused: false, reason: null, since: null, resumeAfter: null });
  assert.equal(next.error.code, "source-window-placement");
});

test("lab helper repair is a one-service transaction and self-tests before writing its receipt", () => {
  const repair = readFileSync(new URL("../../host/scripts/repair-lab-helper.ps1", import.meta.url), "utf8");
  assert.match(repair, /BubblePlayLabHelper/);
  assert.match(repair, /bubbleplay_lab_helper\.exe/);
  assert.match(repair, /stream-lab-helper-probe\.mjs/);
  assert.match(repair, /Get-FileHash[^\r\n]+SHA256/);
  assert.match(repair, /probePassed/);
  assert.doesNotMatch(repair, /BubblePlayBroker|BubblePlayTransport|bubbleplay_agent|bubbleplay-host\.exe|update-service/);
});
import { validateRunId } from "../tools/stream-lab-runtime.mjs";

test("Helper run IDs accept a bare UUID and reject decorated UUIDs", () => {
  const canonical = "c0734ff0-fb0d-4536-9da0-d3943f02fa96";
  assert.doesNotThrow(() => validateRunId(canonical));
  assert.throws(() => validateRunId(`update-probe-${canonical}`), /canonical UUID/);
});

test("receiver profile is persistent and scoped under LOCALAPPDATA", () => {
  const profile = persistentReceiverProfile({ LOCALAPPDATA: "C:\\Users\\Tester\\AppData\\Local" });
  assert.equal(profile, "C:\\Users\\Tester\\AppData\\Local\\BubblePlay\\StreamLab\\Chrome");
});

test("a completed but rejected campaign exits nonzero", () => {
  assert.equal(reportExitCode({ result: "rejected", accepted: false }), 1);
  assert.equal(reportExitCode({ result: "complete", accepted: true }), 0);
  assert.equal(reportExitCode({ result: "unavailable" }), 2);
});

test("preflight rejects a missing helper needed by live startup", () => {
  const root = mkdtempSync(join(tmpdir(), "bubbleplay-preflight-"));
  try {
    const report = runPreflight({
      env: { ...process.env, LOCALAPPDATA: root },
      commandProbe: () => ({ available: true, version: "fixture" }),
      chromeFinder: () => "chrome-fixture.exe",
      serviceProbe: () => ({ installed: false, queryable: true, state: null, stateName: "missing" }),
    });
    assert.equal(report.result, "unavailable");
    assert.deepEqual(report.missing, ["lab-helper-service"]);
    assert.equal(report.labService.installed, false);
    assert.match(report.receiverProfile, /BubblePlay[\\/]StreamLab[\\/]Chrome$/);
    assert.ok(report.dependencies.every((dependency) => dependency.available));
  } finally {
    rmSync(root, { recursive: true, force: true });
  }
});

test("preflight rejects denied helper access needed to verify cleanup", () => {
  const report = runPreflight({
    commandProbe: () => ({ available: true, version: "fixture" }),
    chromeFinder: () => "chrome-fixture.exe",
    serviceProbe: () => ({ installed: null, queryable: false, state: null,
      error: { code: "service-query-denied", message: "query denied" } }),
  });
  assert.deepEqual(report.missing, ["lab-helper-service-access"]);
  assert.equal(report.result, "unavailable");
  assert.equal(report.labService.error.code, "service-query-denied");
});

test("optimization preflight fails closed when the physical activity guard is absent", () => {
  const report = runPreflight({
    commandProbe: () => ({ available: true, version: "fixture" }),
    chromeFinder: () => "chrome-fixture.exe",
    serviceProbe: () => ({ installed: true, queryable: true, state: 1, stateName: "stopped" }),
    requireActivityGuard: true,
    activityGuardPath: "Z:\\missing\\bubbleplay_lab_activity_guard.exe",
  });
  assert.equal(report.result, "unavailable");
  assert.ok(report.missing.includes("physical-activity-guard"));
});

test("a helper pipe that closes without a reply rejects immediately", async () => {
  const server = createServer(socket => socket.once("data", () => socket.destroy()));
  await new Promise(resolve => server.listen(0, "127.0.0.1", resolve));
  const started = Date.now();
  try {
    await assert.rejects(sendHelperRequest("end", {
      runId: "123e4567-e89b-42d3-a456-426614174000",
    }, { pipe: server.address(), timeoutMs: 2_000 }), /closed/i);
    assert.ok(Date.now() - started < 1_000);
  } finally {
    await new Promise(resolve => server.close(resolve));
  }
});

test("the live runner owns one demand-started helper session without importing the CLI or elevation paths", () => {
  const live = readFileSync(new URL("../tools/stream-lab-live.mjs", import.meta.url), "utf8");
  assert.match(live, /from "\.\/stream-lab-runtime\.mjs"/);
  assert.match(live, /startCandidateHost\(\)/);
  assert.match(live, /openDemandStartedHelperSession/);
  assert.match(live, /this\.helperSession/);
  assert.match(live, /this\.helperSession\.renew\(this\.runId, 600\)/);
  assert.match(live, /this\.helperSession\.end\(this\.runId\)/);
  assert.doesNotMatch(live, /from "\.\/stream-lab\.mjs"/);
  assert.doesNotMatch(live, /Start-Process|Verb RunAs|elevateHelper|helperPath/);
});

test("the installer exposes only query/start rights on a demand-start lab service", () => {
  const installer = readFileSync(new URL("../../host/scripts/install-service.ps1", import.meta.url), "utf8");
  assert.match(installer, /Register-BubblePlayLabService/);
  assert.match(installer, /start=',\s*'demand'/);
  assert.match(installer, /Set-LabServiceDacl\s+\$labOwnerSid/);
  assert.match(installer, /\(A;;LCRP;;;\$OwnerSid\)/);
  assert.doesNotMatch(installer, /\$LabLaunchUserSid|Start-Process[^\r\n]+Verb RunAs/);
});

test("the authorized lab owner can read bounded service diagnostics without elevation", () => {
  const installer = readFileSync(new URL("../../host/scripts/install-service.ps1", import.meta.url), "utf8");
  const collector = readFileSync(new URL("../../host/scripts/collect-diagnostics.ps1", import.meta.url), "utf8");
  assert.match(installer, /Set-BubblePlayAcls\s+\$labOwnerSid/);
  assert.match(installer, /Set-ExactDirectoryAcl\s+\$systemLogs[^\r\n]+\$LabOwnerSid/);
  assert.match(installer, /Set-ExactDirectoryAcl\s+\$transportLogs[^\r\n]+\$LabOwnerSid/);
  assert.match(installer, /Set-ExactFileAcl\s+\$_\.FullName[^\r\n]+\$LabOwnerSid/);
  assert.match(collector, /ValidateRange\(1,\s*5000\)/);
  assert.match(collector, /agent\.stderr\.log/);
  assert.match(collector, /lab-helper\.combined\.log/);
  assert.match(collector, /transport-host\.stderr\.log/);
  assert.doesNotMatch(collector, /Start-Process|Verb\s+RunAs|Assert-Administrator/);
});

test("the updater preserves spaced paths through an encoded elevated command", () => {
  const updater = readFileSync(new URL("../../host/scripts/update-service.ps1", import.meta.url), "utf8");
  assert.match(updater, /\[Text\.Encoding\]::Unicode\.GetBytes\(\$elevatedCommand\)/);
  assert.match(updater, /'-EncodedCommand',\s*\$encodedCommand/);
  assert.doesNotMatch(updater, /'-File',\s*\('\"'\s*\+\s*\$installer/);
});

test("the updater trusts a nonce-bound elevated receipt over launcher ambiguity", () => {
  const updater = readFileSync(new URL("../../host/scripts/update-service.ps1", import.meta.url), "utf8");
  assert.match(updater, /update-receipts/);
  assert.match(updater, /receiptId/);
  assert.match(updater, /Write-BubblePlayUpdateReceipt 'started'/);
  assert.match(updater, /Write-BubblePlayUpdateReceipt 'success'/);
  assert.match(updater, /Write-BubblePlayUpdateReceipt 'failed'/);
  assert.match(updater, /Wait-BubblePlayUpdateReceipt/);
  assert.match(updater, /\$receipt\.receiptId -ne \$ReceiptId/);
});

test("service update preflight emits a short-lived hash-bound ready manifest only after full gates", () => {
  const preflight = readFileSync(new URL("../../host/scripts/preflight-service-update.ps1", import.meta.url), "utf8");
  assert.match(preflight, /cargo(?:\.exe)?['"]?\s+test\s+--all-targets/i);
  assert.match(preflight, /npm(?:\.cmd)?['"]?\s+test/i);
  assert.match(preflight, /check:syntax/);
  assert.match(preflight, /cargo(?:\.exe)?['"]?\s+build\s+--release\s+--bins/i);
  assert.match(preflight, /Get-FileHash[^\r\n]+SHA256/i);
  assert.match(preflight, /expiresAt/);
  assert.match(preflight, /ready\s*=\s*\$true/);
  assert.match(preflight, /update-preflight/);
  assert.match(preflight, /collect-diagnostics\.ps1/);
  assert.match(preflight, /BubblePlayLabHelper/);
  assert.match(preflight, /bubbleplay_input_proxy\.exe/);
  const updater = readFileSync(new URL("../../host/scripts/update-service.ps1", import.meta.url), "utf8");
  const installer = readFileSync(new URL("../../host/scripts/install-service.ps1", import.meta.url), "utf8");
  assert.match(updater, /bubbleplay_input_proxy\.exe/);
  assert.match(installer, /bubbleplay_input_proxy\.exe/);
  assert.doesNotMatch(preflight, /Get-Content\s+-Raw\s+-LiteralPath\s+\$hostLog/i);
  assert.match(preflight, /findstr(?:\.exe)?['"]?\s+['"]?\/M/i,
    "an aged-out registration baseline must use the constant-memory first-match probe");
  assert.match(preflight,
    /\$gateErrorActionPreference\s*=\s*\$ErrorActionPreference[\s\S]*\$ErrorActionPreference\s*=\s*'Continue'[\s\S]*&\s+\$Executable\s+@Arguments\s+2>&1[\s\S]*\$ErrorActionPreference\s*=\s*\$gateErrorActionPreference/,
    "native compiler warnings must be captured without bypassing exit-code validation");
});

test("manifest updater is a two-phase transaction with bounded rollback", () => {
  const updater = readFileSync(new URL("../../host/scripts/update-service.ps1", import.meta.url), "utf8");
  assert.match(updater, /ApplyManifest/);
  assert.match(updater, /awaiting-external-validation/);
  assert.match(updater, /validation-result\.json/);
  assert.match(updater, /rolled-back/);
  assert.match(updater, /expiresAt/);
  assert.match(updater, /Get-FileHash[^\r\n]+SHA256/i);
  assert.match(updater, /stream-lab-helper-probe\.mjs/);
  assert.match(updater, /Get-NetQosPolicy/);
  assert.match(updater, /rollback/i);
  assert.doesNotMatch(updater, /Get-Content\s+-Raw\s+-LiteralPath\s+\$hostLog/i,
    "multi-gigabyte service logs must never be loaded into updater memory");
  assert.match(updater, /Get-Content\s+-LiteralPath\s+\$Path\s+-Tail\s+\d+/i,
    "post-restart registration must be proven from a bounded log tail");
  assert.match(updater, /Test-BubblePlayRecentHostRegistration\s+\$hostLog/i);
});

test("the first input-proxy rollout preserves a complete rollback for an additive binary", () => {
  const updater = readFileSync(new URL("../../host/scripts/update-service.ps1", import.meta.url), "utf8");
  assert.match(updater,
    /\$additiveMigrationBinaries\s*=\s*@\(\s*'bubbleplay_input_proxy\.exe'\s*\)/i,
    "only the explicitly additive input proxy may be absent from the old installation");
  assert.match(updater, /\$absentBefore\s*=\s*\[Collections\.Generic\.List\[string\]\]::new\(\)/i);
  assert.match(updater,
    /if\s*\(\$name\s+-notin\s+\$additiveMigrationBinaries\)\s*\{\s*throw\s+"Installed rollback source is missing:/i,
    "a missing pre-existing core binary must still fail closed");
  assert.match(updater,
    /\$absentBefore\.Add\(\$name\)[\s\S]*Copy-Item[^\r\n]+\$manifestSource[^\r\n]+\$rollbackSource/i,
    "the rollback source must remain installer-complete without inventing an old proxy binary");
  assert.match(updater,
    /foreach\s*\(\$name\s+in\s+\$absentBefore\)[\s\S]*Remove-Item\s+-Force\s+-LiteralPath\s+\$installed/i,
    "rollback must restore the pre-update absence of additive binaries");
});

test("the privileged helper probe exercises a real no-VDD begin/end lifecycle", () => {
  const probe = readFileSync(new URL("../tools/stream-lab-helper-probe.mjs", import.meta.url), "utf8");
  assert.match(probe, /openDemandStartedHelperSession/);
  assert.match(probe, /const\s+runId\s*=\s*randomUUID\(\)/);
  assert.match(probe, /validateRunId\(runId\)/);
  assert.doesNotMatch(probe, /`[^`]*\$\{randomUUID\(\)\}/);
  assert.match(probe, /vdd:\s*false/);
  assert.match(probe, /session\.end\(runId\)/);
  assert.doesNotMatch(probe, /Start-Process|Verb\s+RunAs|elevat/i);
});

test("external validation publisher is nonce and manifest bound", () => {
  const publisher = readFileSync(new URL("../../host/scripts/publish-update-validation.ps1", import.meta.url), "utf8");
  assert.match(publisher, /receiptId/);
  assert.match(publisher, /validationNonce/);
  assert.match(publisher, /manifestSha256/);
  assert.match(publisher, /validation-result\.json/);
  assert.doesNotMatch(publisher, /Start-Process|Verb\s+RunAs|Assert-Administrator/);
});

test("external service validation is logged, bounded, and publishes pass or rollback evidence", () => {
  const validator = readFileSync(new URL("../../host/scripts/validate-service-update.ps1", import.meta.url), "utf8");
  assert.match(validator, /SmokeRuns\s*=\s*3/);
  assert.match(validator, /MaxSmokeAttempts\s*=\s*9/);
  assert.match(validator, /SignalingSeconds\s*=\s*600/);
  assert.match(validator, /CleanupSeconds\s*=\s*60/);
  assert.match(validator, /EnvironmentOnly/);
  assert.match(validator, /ParseSmokeLog/);
  assert.match(validator, /Read-BubblePlayCliJson/);
  assert.match(validator, /@\(Get-LabChromeProcesses\)\.Count/);
  assert.match(validator, /stream-lab-helper-probe\.mjs/);
  assert.match(validator, /collect-diagnostics\.ps1/);
  assert.match(validator, /\$labCli\s*=.*stream-lab\.mjs/);
  assert.match(validator, /&\s*node\.exe\s+\$labCli\s+['"]smoke['"]/);
  assert.match(validator, /WorkerVersionId/);
  assert.match(validator, /['"]--worker-version-id['"]\s+\$WorkerVersionId/);
  assert.match(validator, /['"]--no-action-probe['"]/);
  assert.match(validator,
    /\$smokeErrorActionPreference\s*=\s*\$ErrorActionPreference[\s\S]*\$ErrorActionPreference\s*=\s*['"]Continue['"][\s\S]*&\s+node\.exe\s+\$labCli\s+['"]smoke['"][\s\S]*\$ErrorActionPreference\s*=\s*\$smokeErrorActionPreference/,
    "Stream Lab stderr progress must be logged without becoming a terminating PowerShell error");
  assert.match(validator, /publish-update-validation\.ps1/);
  assert.match(validator, /-Result\s+pass/);
  assert.match(validator, /-Result\s+fail/);
  assert.doesNotMatch(validator, /Start-Process|Verb\s+RunAs|elevat/i);
});

test("external validation admits bounded Host child recovery while the Transport service remains stable", () => {
  const validator = readFileSync(new URL("../../host/scripts/validate-service-update.ps1", import.meta.url), "utf8");
  assert.match(validator, /\$MaxHostRecoveries\s*=\s*3/);
  assert.match(validator, /Test-BubblePlayRecentHostRegistration/);
  assert.match(validator, /Get-Content\s+-LiteralPath\s+\$Path\s+-Tail\s+3000/i,
    "re-registration evidence must use a bounded log tail");
  assert.match(validator,
    /\$HostRecoveries\.Count\s+-ge\s+\$MaxHostRecoveries[\s\S]*throw[^\r\n]+recovery budget/i,
    "repeated child flapping must still fail validation");
  assert.match(validator, /Add-Evidence\s+'host-recovery'\s+'pass'/i);
  assert.match(validator, /BubblePlayTransport stopped or restarted during external validation/);
});

test("encoder rounds vary one dimension around the champion", () => {
  const champion = { preset: "p3", tuning: "ultra-low-latency", multipass: "disabled", vbvFrames: 1, aqStrength: 8, temporalAq: false, gopSeconds: 2 };
  const round = buildEncoderRound(champion, "vbvFrames");
  assert.deepEqual(round.map((candidate) => candidate.vbvFrames), [1, 2, 4]);
  assert.ok(round.every((candidate) => candidate.preset === "p3" && candidate.aqStrength === 8));
  assert.deepEqual(buildEncoderRound(champion, "gopSeconds").map(candidate => candidate.gopSeconds), [1, 2, 4]);
});

test("text-edge screening selects exactly one default-off ROI experiment axis", () => {
  assert.deepEqual(buildScreeningCandidate(["--roi-mode", "text-edge"]), {
    id: "roi-text-edge",
    schemaVersion: 2,
    implementationVariant: "candidate",
    experimentId: "roi-text-edge",
    preset: "p3",
    tuning: "ultra-low-latency",
    multipass: "disabled",
    aqMode: "spatial",
    aqStrength: 8,
    vbvFrames: 1,
    lookaheadFrames: 0,
    weightedPrediction: false,
    dirtyQpMap: false,
    main10: false,
    roiMode: "text-edge",
    resourceMode: "per-frame",
    asyncDepth: 0,
    hybridMode: "off",
    gopSeconds: 2,
    bitrateMbps: 6,
  });
  assert.throws(() => buildScreeningCandidate(["--roi-mode", "text-edge", "--dirty-qp-map"]),
    /separate hypotheses/i);
});

test("feedback-headroom screening selects a distinct default-off ROI policy", () => {
  const candidate = buildScreeningCandidate(["--roi-mode", "feedback-headroom"]);
  assert.equal(candidate.id, "roi-feedback-headroom");
  assert.equal(candidate.experimentId, "roi-feedback-headroom");
  assert.equal(candidate.roiMode, "feedback-headroom");
  assert.equal(candidate.bitrateMbps, 6);
});

test("hybrid screening selects only the PNG static-detail experiment axis", () => {
  const candidate = buildScreeningCandidate(["--hybrid-mode", "png-static-roi"]);
  assert.equal(candidate.id, "hybrid-png-static-roi");
  assert.equal(candidate.experimentId, "hybrid-png-static-roi");
  assert.equal(candidate.hybridMode, "png-static-roi");
  assert.equal(candidate.roiMode, "off");
  assert.equal(candidate.resourceMode, "per-frame");
  assert.equal(candidate.asyncDepth, 0);
  assert.throws(() => buildScreeningCandidate([
    "--hybrid-mode", "png-static-roi", "--roi-mode", "damage",
  ]), /separate hypotheses/i);
});

test("campaign plan declares the shaped baseline matrix, GOP search, long soak, and stability units", () => {
  const plan = encoderCampaignPlan();
  assert.deepEqual(plan.baselineMatrix, [
    { bandwidthMbps: null, repetitions: 1 }, { bandwidthMbps: 2, repetitions: 1 },
    { bandwidthMbps: 4, repetitions: 3 }, { bandwidthMbps: 8, repetitions: 1 },
    { bandwidthMbps: 12, repetitions: 1 },
  ]);
  assert.ok(plan.dimensions.includes("gopSeconds"));
  assert.equal(plan.soakSeconds, 600);
  assert.equal(plan.adaptiveDwellSeconds, 120);
  assert.deepEqual(plan.networkPolicy, { encoderScreening: "unshaped", constrainedHeadroom: { wss: 0.65, webrtc: 0.90 } });
  assert.deepEqual(plan.stabilityUnits.map(unit => unit.id), [
    "decoder-fault-recovery", "sharp-viewport-2x", "adaptive-bandwidth-staircase", "wss-gaming", "webrtc-gaming-unshaped",
    "webrtc-gaming-relay-8m", "h264-fallback-smoke", "post-cleanup-idle",
  ]);
});

test("aggressive optimization plan uses repeated baselines and a parity frontier", () => {
  const plan = optimizationCampaignPlan();
  assert.equal(plan.strategy, "parity-pareto-loop");
  assert.deepEqual(plan.baselineMatrix.map(item => item.repetitions), [3, 3, 3, 3, 3]);
  assert.equal(plan.stopAfterRejectedHypotheses, 3);
  assert.equal(plan.resumeIdleSeconds, 600);
  assert.equal(plan.autoPauseOnPhysicalActivity, true);
  assert.equal(plan.parameterCandidates, 96);
  assert.deepEqual(plan.abbaOrder, ["production", "candidate", "candidate", "production"]);
});

test("helper messages match the bounded NDJSON lease protocol", () => {
  const runId = "123e4567-e89b-42d3-a456-426614174000";
  assert.equal(STREAM_LAB_PIPE, "\\\\.\\pipe\\BubblePlay.StreamLab.v1");
  assert.deepEqual(buildHelperRequest("begin", { runId, leaseSeconds: 600, vdd: true }), {
    command: "begin", runId, leaseSeconds: 600, vdd: true,
  });
  assert.deepEqual(buildHelperRequest("renew", { runId, leaseSeconds: 600 }), {
    command: "renew", runId, leaseSeconds: 600,
  });
  assert.deepEqual(buildHelperRequest("shape", { runId, leaseSeconds: 600, bandwidthMbps: null }), {
    command: "shape", runId, leaseSeconds: 600, bandwidthMbps: null,
  });
  assert.throws(() => buildHelperRequest("shape", { runId, leaseSeconds: 600, bandwidthMbps: 3 }), /bandwidth/);
  assert.deepEqual(buildHelperRequest("apply", {
    runId, leaseSeconds: 60, bandwidthMbps: 4,
    profile: {
      schemaVersion: 1, transport: "wss", codec: "hevc", streamMode: "work",
      adaptive: false, width: 1920, height: 1080, fps: 30, bitrateBps: 4_000_000, gopSeconds: 2,
      encoder: { preset: "p4", tuning: "low-latency", multipass: "quarter", aqStrength: 8,
        temporalAq: true, vbvFrames: 2 },
    },
  }).command, "apply");
  const game = buildHelperRequest("apply", {
    runId, leaseSeconds: 60, bandwidthMbps: 8,
    profile: { schemaVersion: 1, transport: "wss", codec: "hevc", streamMode: "game", adaptive: false,
      width: 1920, height: 1080, fps: 60, bitrateBps: 8_000_000, gopSeconds: 1,
      encoder: { preset: "p3", tuning: "ultra-low-latency", multipass: "disabled", aqStrength: 8, temporalAq: false, vbvFrames: 1 } },
  });
  assert.equal(game.profile.codec, "hevc");
  assert.equal(game.profile.streamMode, "game");
  assert.equal(game.profile.gopSeconds, 1);
  const v2 = buildHelperRequest("apply", {
    runId, leaseSeconds: 60, bandwidthMbps: 12,
    profile: { schemaVersion: 2, implementationVariant: "candidate", experimentId: "enc-work-096",
      transport: "wss", codec: "hevc", streamMode: "work", adaptive: false,
      width: 1920, height: 1080, fps: 30, bitrateBps: 12_000_000, gopSeconds: 4,
      encoder: { preset: "p7", tuning: "low-latency", multipass: "full", aqMode: "temporal",
        aqStrength: 15, vbvFrames: 4, lookaheadDepth: 2, weightedPrediction: true, dirtyQpMap: false, main10: true } },
  });
  assert.equal(v2.profile.schemaVersion, 2);
  assert.equal(v2.profile.encoder.lookaheadDepth, 2);
  assert.equal(v2.profile.encoder.weightedPrediction, true);
  assert.equal(v2.profile.encoder.dirtyQpMap, false);
  assert.equal(v2.profile.encoder.main10, true);
  assert.throws(() => buildHelperRequest("apply", { runId, leaseSeconds: 60, bandwidthMbps: 8,
    profile: { ...game.profile, codec: "h264" } }), /hevc/i);
  assert.throws(() => buildHelperRequest("apply", { runId, leaseSeconds: 60, bandwidthMbps: 8,
    profile: { ...game.profile, streamMode: "work", fps: 60 } }), /fps/i);
  assert.throws(() => buildHelperRequest("apply", { ...game, runId, leaseSeconds: 60, bandwidthMbps: 8,
    profile: { ...game.profile, gopSeconds: 3 } }), /gop/i);
  assert.throws(() => buildHelperRequest("begin", { runId: "not-a-uuid", leaseSeconds: 60, vdd: true }), /runId/);
  assert.throws(() => buildHelperRequest("apply", { runId, leaseSeconds: 60, bandwidthMbps: 3, profile: {} }), /bandwidth/);
});

test("preflight verifies every required scoring binary and treats GPU telemetry as optional", () => {
  const calls = [];
  const report = runPreflight({
    commandProbe: (file, args) => {
      calls.push([file, ...args]);
      if (file === "nvidia-smi") return { available: false, version: null };
      return { available: true, version: "fixture" };
    },
    chromeFinder: () => "chrome.exe",
    serviceProbe: () => ({ installed: true, queryable: true, state: 1, stateName: "stopped" }),
  });
  assert.ok(calls.some(call => call[0] === "ffprobe"));
  assert.ok(calls.some(call => call[0] === "ffmpeg" && call.includes("-filters")));
  assert.equal(report.result, "ready");
  assert.equal(report.optionalTelemetry.find(item => item.name === "nvidia-smi").available, false);
});

test("preflight rejects FFmpeg builds without libvmaf", () => {
  const report = runPreflight({
    commandProbe: (file, args, validator = () => true) => ({
      available: file === "ffmpeg" ? validator("blurdetect ssim psnr") : true,
      version: "fixture",
    }),
    chromeFinder: () => "chrome.exe",
    serviceProbe: () => ({ installed: true, queryable: true, state: 1, stateName: "stopped" }),
  });
  assert.ok(report.missing.includes("ffmpeg-libvmaf"));
});

test("compare CLI reads metrics and persists its decision", () => {
  const root = mkdtempSync(join(tmpdir(), "bubbleplay-compare-"));
  try {
    const baseline = join(root, "baseline.json");
    const candidate = join(root, "candidate.json");
    writeFileSync(baseline, JSON.stringify(fixture({ id: "baseline", ocrCer: 0.08 })));
    writeFileSync(candidate, JSON.stringify(fixture({ id: "candidate", ocrCer: 0.05 })));
    const result = spawnSync(process.execPath, [
      new URL("../tools/stream-lab.mjs", import.meta.url).pathname.slice(1),
      "compare", baseline, candidate, "--output", join(root, "comparison.json"),
    ], { encoding: "utf8" });
    assert.equal(result.status, 0, result.stderr);
    assert.equal(JSON.parse(result.stdout).accepted, true);
    assert.equal(JSON.parse(readFileSync(join(root, "comparison.json"), "utf8")).candidateId, "candidate");
  } finally {
    rmSync(root, { recursive: true, force: true });
  }
});

test("cleanup removes only expired run-ID directories with matching leases", () => {
  const root = mkdtempSync(join(tmpdir(), "bubbleplay-cleanup-"));
  const env = { LOCALAPPDATA: root };
  const runs = join(root, "BubblePlay", "StreamLab", "runs");
  try {
    for (const [name, expiresAt, leaseId = name] of [
      ["expired-run", "2020-01-01T00:00:00.000Z"],
      ["active-run", "2099-01-01T00:00:00.000Z"],
      ["mismatch-run", "2020-01-01T00:00:00.000Z", "somebody-else"],
    ]) {
      mkdirSync(join(runs, name), { recursive: true });
      writeFileSync(join(runs, name, "lease.json"), JSON.stringify({ runId: leaseId, expiresAt }));
    }
    mkdirSync(join(runs, "no-lease"), { recursive: true });
    const report = cleanLocalExpiredRuns(env, Date.parse("2026-01-01T00:00:00.000Z"));
    assert.deepEqual(report.removed, ["expired-run"]);
    assert.equal(existsSync(join(runs, "expired-run")), false);
    assert.equal(existsSync(join(runs, "active-run")), true);
    assert.equal(existsSync(join(runs, "mismatch-run")), true);
    assert.equal(existsSync(join(runs, "no-lease")), true);
  } finally { rmSync(root, { recursive: true, force: true }); }
});

function fixture(overrides = {}) {
  return {
    id: "candidate", transport: "wss", codec: "hevc", firstFrameMs: 1_000,
    presentedFps: 30, encodeP95Ms: 5, latencyP95Ms: 300, actionToPhotonP95Ms: 300,
    freezes: 0, decoderResets: 0, dependencyLosses: 0, spatialTransitions: 0,
    queueSlopePerMinute: 0, memorySlopeMbPerMinute: 0, ocrCer: 0.08,
    worstTileSsim: 0.8, vmaf: 80, wireBitrateMbps: 4, ...overrides,
  };
}

test("package and CLI expose the manual and optimize commands", () => {
  const packageJson = JSON.parse(readFileSync(new URL("../package.json", import.meta.url), "utf8"));
  const cli = readFileSync(new URL("../tools/stream-lab.mjs", import.meta.url), "utf8");
  assert.equal(packageJson.scripts["lab:manual"], "node tools/stream-lab.mjs manual");
  assert.equal(packageJson.scripts["lab:optimize"], "node tools/stream-lab.mjs optimize");
  assert.match(cli, /\["manual", "baseline", "smoke", "sweep", "campaign", "optimize"\]/);
  assert.match(cli, /createCampaignStateV2\(\{ runId \}\)/);
  assert.match(cli, /runLiveOptimize/);
  assert.match(cli, /runLiveManual/);
  assert.match(cli, /args\.includes\("--main10"\)/);
  assert.match(cli, /experimentId:\s*"hevc-main10"/);
});
