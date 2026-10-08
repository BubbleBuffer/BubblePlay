// Configuration is validated before admitting any browser: there is no
// password-only fallback when a notifier is misspelled or unconfigured.
export function approvalMethods(env) {
  const configured = env.APPROVAL_METHODS === undefined ? "ntfy" : env.APPROVAL_METHODS;
  if (typeof configured !== "string") throw new Error("Invalid APPROVAL_METHODS");
  const methods = configured.split(",").map(value => value.trim());
  if (methods.some(value => !["ntfy", "host"].includes(value)))
    throw new Error("APPROVAL_METHODS must contain ntfy or host");
  if (methods.includes("ntfy") && ["NTFY_URL", "NTFY_TOPIC", "NTFY_TOKEN"].some(key =>
    typeof env[key] !== "string" || !env[key].trim()))
    throw new Error("ntfy requires NTFY_URL, NTFY_TOPIC and NTFY_TOKEN");
  return [...new Set(methods)];
}
