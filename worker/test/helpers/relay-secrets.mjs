// Placeholder credentials that satisfy the relay's minimum secret lengths
// (32 characters for tokens, 12 for the password). Not real secrets.
export const RELAY_SECRETS = Object.freeze({
  TOKEN: "test-relay-token-0123456789abcdef",
  HOST_GATE_TOKEN: "test-host-gate-token-0123456789ab",
  GATE_ADMIN_TOKEN: "test-gate-admin-token-0123456789a",
  APP_PASSWORD: "correct horse battery staple",
});
