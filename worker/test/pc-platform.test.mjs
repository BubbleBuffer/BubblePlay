import test from "node:test";
import assert from "node:assert/strict";
import worker from "../src/index.js";
import { RELAY_SECRETS } from "./helpers/relay-secrets.mjs";

test("default PC selection preserves the browser upgrade and version routing headers", async () => {
  const pc = "77VH-7M56-8GJK-Y12J";
  const names = [];
  const result = new Response("room");
  const headers = {
    Upgrade: "websocket", Origin: "https://play.example", Cookie: "synthetic=cookies",
    "Cloudflare-Workers-Version-Overrides": 'relay="synthetic-version"',
  };
  const env = {
    ...RELAY_SECRETS, APPROVAL_METHODS: "host",
    GATE: { idFromName: name => name, get: () => ({ fetch: async request => {
      if (new URL(request.url).pathname === "/__pc/resolve") return Response.json({ pc });
      assert.equal(request.headers.get("Cookie"), headers.Cookie);
      assert.equal(request.headers.get("Cloudflare-Workers-Version-Overrides"), headers["Cloudflare-Workers-Version-Overrides"]);
      return new Response(null, { status: 204 });
    } }) },
    ROOM: { idFromName: name => { names.push(name); return name; }, get: () => ({ fetch: async request => {
      assert.equal(new URL(request.url).searchParams.get("pc"), pc);
      for (const [name, value] of Object.entries(headers)) assert.equal(request.headers.get(name), value, name);
      return result;
    } }) },
  };
  assert.equal(await worker.fetch(new Request("https://play.example/ws", { headers }), env), result);
  assert.deepEqual(names, [`pc-v1:${pc}`]);
});
