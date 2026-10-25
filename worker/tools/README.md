# Worker tools

[lab/](lab/) contains development experiments, local relays, browser automation
and measurement helpers. It is separate from normal Worker setup. The existing
`npm run lab:*`, `npm run latency:*` and `npm run recovery:inventory` commands
continue to resolve the lab tools from the Worker directory.
