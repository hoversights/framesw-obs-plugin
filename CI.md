# CI

## How it works

`.github/workflows/build.yml` runs on every `push` to this repo (any
branch) — **never** on `pull_request`/`pull_request_target`. That's
deliberate: this is a public repo, and a fork-triggered `pull_request`
run would execute workflow code (with access to this repo's secrets,
if a workflow ever used them on that trigger) against arbitrary PR
content. Signing secrets and fork-triggerable workflows never mix in
this repo — if you ever add a `pull_request` workflow here (linting,
etc.), keep it entirely separate from `build.yml` and give it no
secrets access.

Two jobs run in parallel, both on GitHub-hosted runners (`macos-latest`,
`windows-latest` — no self-hosted runner is used or should ever be
registered for this repo):

- **macos**: builds a universal (arm64 + x86_64) release binary via
  `package-macos.sh --release`, signs it with the Developer ID
  Application certificate (imported into a throwaway keychain that's
  deleted at the end of the job, `if: always()`, so signing material
  never lingers on disk past the run) using hardened runtime + a secure
  timestamp.
- **windows**: builds the DLL via `package-windows.ps1`, then signs it
  with **Azure Artifact Signing** (the product formerly called Azure
  Trusted Signing) using the `hoversights-public-trust` profile, and
  verifies the result with `signtool verify /pa` before packaging. Both
  the trusted-chain check and the presence of a timestamp are hard
  build failures — see "Windows signing" below.

Both jobs run `cargo test --locked` first. There are currently **zero
unit tests** in this crate — this step is a no-op today, wired up so
tests added later run automatically. No vendor request can be covered by
CI regardless: those paths only execute inside a live OBS process
reached over obs-websocket, nothing here simulates that, and nothing
should.

## Windows signing

Signing runs on **every push**, matching the macOS job. There is no
branch condition, deliberately — an unsigned artifact escaping unnoticed
is the failure this exists to prevent.

**The certificate profile must never be recreated.** SmartScreen
reputation accrues to the signing identity, not to the publisher or the
product. Deleting `hoversights-public-trust` and creating a replacement —
even with an identical name and subject — restarts that reputation at
zero, and users get "Windows protected your PC" again until it rebuilds.
If signing looks broken, fix whatever consumes the profile. Note the
profile in use today was itself created 2026-08-05, so its reputation is
young; that is all the more reason not to reset it again.

The profile is **Public Trust**, not "Public Trust Test" and not
"Private Trust". The Test variant chains to a root Windows does not
trust: it produces a signature that verifies under permissive flags but
is rejected on a real user's machine — CI green, every download broken.
That is why the verification step uses `/pa`, the same Default
Authenticode Verification Policy Windows itself applies, rather than a
weaker check that a Test-profile signature would pass.

The timestamp check is separate and equally load-bearing. Artifact
Signing issues certificates valid for only ~3 days by design; RFC-3161
timestamping is what keeps an already-shipped binary trusted afterwards.
An untimestamped signature passes `/pa` on the day it is made and starts
failing days later, long after the run went green — so the workflow
asserts a timestamp is present rather than assuming the signing step
attached one.

The Rust toolchain version is pinned via `rust-toolchain.toml` at the
repo root (not floating `stable`) — rustup resolves it automatically on
the first `cargo`/`rustup` invocation in either job.

## Where artifacts land

Each job uploads one zip as a workflow artifact (Actions → the specific
run → Artifacts), named:

```
framesw-companion-<version>-<shortSHA>-macos.zip
framesw-companion-<version>-<shortSHA>-windows.zip
```

`<version>` comes from `Cargo.toml`'s `version` field (the single
source of truth — bump it there, nothing else needs updating for
artifact naming to follow). `<shortSHA>` is the first 7 characters of
the commit SHA that triggered the run.

**Retention is 90 days, set explicitly** (`retention-days: 90` in the
workflow — matches this org's current default, but pinned rather than
left implicit, since GitHub's account-level default can change).
**There is no GitHub Release and no permanent download URL** — if the
app repo needs a specific plugin build older than 90 days, it's gone;
re-run the workflow from that commit (`git checkout <sha> && git push`
to a throwaway branch, or `gh workflow run` — re-running only replays
CI, it doesn't reconstruct an expired artifact) or rebuild locally from
that commit.

## Fetching a plugin artifact from the app repo (`obs_controller`)

Workflow artifacts are **not** publicly downloadable, even on a public
repo — the Actions API always requires authentication for artifact
listing/download, regardless of repo visibility. The app repo's
bundle/packaging job needs a credential with at least **read access to
Actions** on `hoversights/framesw-obs-plugin`.

**Recommended**: a fine-grained PAT scoped to *only* that one repo,
*only* the "Actions: Read-only" repository permission — not a broad
classic PAT. Store it as a secret in the app repo (e.g.
`PLUGIN_REPO_ARTIFACT_TOKEN` — this task does not create it; that's the
app repo's own secret to add, per its own settings).

Steps for the app repo's bundle job (using `gh`, already present on
the self-hosted app-repo runners):

```sh
# Authenticate gh with the read-only PAT for this call
export GH_TOKEN="$PLUGIN_REPO_ARTIFACT_TOKEN"

# Find the run for a specific commit (pin to an exact SHA for a
# reproducible release, not just "latest main"):
RUN_ID=$(gh run list \
  --repo hoversights/framesw-obs-plugin \
  --workflow build.yml \
  --json databaseId,headSha,status,conclusion \
  --jq '[.[] | select(.headSha == "<PINNED_PLUGIN_SHA>" and .conclusion == "success")][0].databaseId')

# Download both platform artifacts (name must match exactly, including
# version+SHA — list them first with `gh run view $RUN_ID --repo ...`
# if the exact version isn't already known):
gh run download "$RUN_ID" --repo hoversights/framesw-obs-plugin \
  --name "framesw-companion-<version>-<shortSHA>-macos" --dir ./fetched-plugin
gh run download "$RUN_ID" --repo hoversights/framesw-obs-plugin \
  --name "framesw-companion-<version>-<shortSHA>-windows" --dir ./fetched-plugin
```

Unzip and place the contents where the app's packaging scripts expect
the plugin bundle/DLL (`package-macos.sh`/`package-windows.ps1` in
`obs_controller` — see that repo's own scripts for the exact path).
This document does not restructure the `obs-plugin/` submodule or how
the app currently pins/vendors the plugin — it only covers how to
*fetch a specific CI-built artifact* once you've decided to use one.

## Required repo secrets (Settings → Secrets and variables → Actions)

Create these in `hoversights/framesw-obs-plugin`'s own repo settings —
they are separate from, and do not need to match, any secret in the
app repo:

| Secret | Contents |
|---|---|
| `MACOS_CERTIFICATE` | The Developer ID Application `.p12`, base64-encoded (`base64 -i cert.p12 \| pbcopy` on macOS) |
| `MACOS_CERTIFICATE_PWD` | The password used when exporting that `.p12` |
| `KEYCHAIN_PASSWORD` | Any password of your choosing — only ever used to lock/unlock the throwaway keychain created and destroyed within a single job run; not tied to any existing credential |
| `AZURE_TENANT_ID` | The Azure AD tenant (directory) ID for the `hoversights` signing account |
| `AZURE_CLIENT_ID` | Application (client) ID of the CI service principal |
| `AZURE_CLIENT_SECRET` | That service principal's client secret |

**The three Azure values require a service principal, which the local
Windows box does not use.** Local release builds authenticate with an
interactive `az login` as the subscription's own user; a CI runner
cannot. Create one and grant it the signing role:

```sh
az ad sp create-for-rbac --name framesw-plugin-signing-ci
# note appId -> AZURE_CLIENT_ID, password -> AZURE_CLIENT_SECRET,
#      tenant -> AZURE_TENANT_ID

# Then, on the `hoversights` signing account, assign the role:
#   "Artifact Signing Certificate Profile Signer"
# Search IAM for "artifact" — the role was renamed from
# "Trusted Signing Certificate Profile Signer" and a search for
# "trusted" returns nothing.
```

Scope the role assignment to the signing account, not the whole
subscription. Rotate `AZURE_CLIENT_SECRET` on its own expiry — a signing
failure that starts on a specific date with an auth error is almost
always this, not a broken profile, and the fix is a new secret rather
than anything touching the certificate.

The endpoint, account name and certificate profile name are **not**
secrets and are in the workflow in clear text. They identify which
profile signed a binary, which anyone can read out of the signature.
