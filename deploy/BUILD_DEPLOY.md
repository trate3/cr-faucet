# Build & deploy a new image to testnet (the actual, working sequence)

> ⚠️ **Start at [DEPLOY.md](DEPLOY.md)** — it consolidates this and runs the flow via `preflight.sh` + `redeploy.sh`. Where this doc disagrees, DEPLOY.md + the scripts win.

The other runbooks claim `oasis rofl build` "shells out to docker buildx + pushes
the image." **That is wrong for the current CLI (tooling 0.19.0).** `oasis rofl
build` only builds the ROFL *bundle* (`mining-pool.testnet.orc` + enclave
identity); it does NOT build or push the container image. You build + push the
image yourself with `docker buildx`. This doc is the source of truth.

All commands run on THIS host. The `oasis` TUI is driven headless by
`deploy/oasis_wrap.py` (`OASIS_PASS=test`, the `deployer` wallet). See
`HEADLESS_OPS.md` for that wrapper and `ROFL_RUNBOOK.md` for the first-time setup.

## The loop (redeploy with new code)

1. **Edit config + bump the image tag.** In `deploy/compose.yaml` bump
   `image: ghcr.io/trate3/drip-test:vN` (the tag MUST change so the provider's
   containerd re-pulls). Make any `pool.example.toml` / env / `torrc` edits — the
   Dockerfile bakes `deploy/pool.example.toml` → `/etc/pool/pool.toml`.

2. **Build + push the container image** (the ~20-min Rust release build). From the
   **repo root** (the Dockerfile `COPY deploy/...` needs root as the context):
   ```bash
   docker buildx build --platform linux/amd64 \
     -f Dockerfile -t ghcr.io/trate3/drip-test:vN . --push
   ```
   (Already logged into ghcr.io.) This is the step `oasis rofl build` does NOT do.

3. **Build the ROFL bundle** (fast — wraps the image ref in a measured bundle).
   From `deploy/`:
   ```bash
   cd deploy
   OASIS_PASS="" python3 oasis_wrap.py rofl build
   ```
   If it errors `unable to fetch manifest for image ...: not found`, the pushed
   tag hasn't propagated (or you skipped step 2) — re-push, or add `--force` to
   bundle anyway (it references the tag; the machine pulls it at runtime). Output:
   `mining-pool.testnet.orc` + a new enclave identity.

4. **Update the on-chain app config** (enclave allowlist — `rofl build` rewrites
   `rofl.yaml`'s `policy.enclaves` with the new measurement). From `deploy/`:
   ```bash
   OASIS_PASS="" python3 oasis_wrap.py rofl update
   ```
   (`oasis_wrap.py` auto-confirms the "Sign this transaction?" prompt.)

5. **Deploy the new bundle to the machine** — THIS is what actually makes the
   running machine fetch + boot the new build:
   ```bash
   OASIS_PASS="" python3 oasis_wrap.py rofl deploy   # --machine default = the existing 0624
   ```
   ⚠️ Do NOT use `rofl machine restart` for a new build — it just restarts the
   OLD bundle, whose enclave `update` removed from the allowlist, so the machine
   loops on `failed to refresh registration: unknown enclave` and the container
   never starts. `rofl deploy` is the step that pushes the new bundle. (`restart`
   is only for rebooting the *current* build.) Deploy reuses the existing paid
   term; `oasis rofl machine top-up` extends it.

6. **Verify.**
   ```bash
   OASIS_PASS="" python3 oasis_wrap.py rofl machine show     # state + public URLs
   OASIS_PASS="" python3 oasis_wrap.py rofl machine logs     # pool stderr
   ```

## Gotchas (learned the hard way)

- **`rofl build` does not build the image** — step 2 is mandatory and separate.
- **`rofl deploy`, not `rofl machine restart`, ships a new build** (step 5). Restart
  reboots the old bundle → `unknown enclave` loop → container never starts.
- **The oasis wallet passphrase is EMPTY (`OASIS_PASS=""`)**, not `test`.
  (`HEADLESS_OPS.md` says `test` — that's the *forge key-file* pass for
  `deploy/secrets/deployer.json`, a different thing. The oasis CLI wallet, imported
  with an empty passphrase, rejects `test`.)
- **Bump the tag every time** — same-tag pulls won't re-fetch (containerd cache).
- **Context = repo root** for `docker buildx` (the Dockerfile copies `deploy/...`).
- **Tor is installed from the Tor Project apt repo** (Dockerfile), not Debian's —
  Debian bookworm ships tor 0.4.7 which lacks `HiddenServicePoWDefensesEnabled`
  (needs 0.4.8) and would crash on the oracle torrc. The torproject repo gives a
  current tor so both `HiddenServiceExportCircuitID` (per-circuit limiting) and the
  onion PoW work.
- **Sapphire is legacy (type-0) txs only** — `--legacy` on every `forge`/`cast`.
- Contracts in the sibling `crossroads-integration` repo deploy via its
  `script/deploy_signer_registry.sh` (pool, once) + `script/deploy_oracle.sh`.
