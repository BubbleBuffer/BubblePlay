# Worker tools

`set-app-password.ps1` sets the Cloudflare Worker application password through
Wrangler. Follow the [Cloudflare setup guide](../../docs/self-host-cloudflare.md)
for the complete deployment steps. `test-set-app-password.ps1` checks the setup
script without publishing a Worker or changing a password.

[lab/](lab/) contains development experiments, local relays, browser automation
and measurement helpers. It is separate from normal Worker setup. The existing
`npm run lab:*`, `npm run latency:*` and `npm run recovery:inventory` commands
continue to resolve the lab tools from the Worker directory.
