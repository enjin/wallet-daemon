# Enjin Wallet Daemon

A lightweight outbound-only signer for Enjin Platform transactions.

[![License: LGPL 3.0](https://img.shields.io/badge/license-LGPL_3.0-purple)](https://opensource.org/license/lgpl-3-0/)

The daemon listens for authenticated Enjin Platform WebSocket events, fetches pending work through GraphQL, signs it with the configured wallet, and returns signed payloads to the platform for broadcast. A three-minute safety poll covers missed events while the subscription is healthy; if Pusher is unavailable, the daemon automatically resumes six-second polling until the authenticated subscription is restored. It does not accept inbound transaction requests, so the host running the daemon should not expose public ports for daemon traffic.

For the full user guide, including binary downloads, Docker, AWS CloudFormation, import, export, and migration workflows, see [Using the Wallet Daemon](https://docs.enjin.io/getting-started/using-wallet-daemon).

## Security Notes

The daemon is security-sensitive.

- It stores an encrypted `wallet.seed` file on disk.
- `KEY_PASS` encrypts and decrypts `wallet.seed`.
- `PLATFORM_KEY` authenticates the daemon with Enjin Platform.

Back up `wallet.seed` and `KEY_PASS` separately. Losing either value can make the wallet unrecoverable. Do not rotate `KEY_PASS` for an existing `wallet.seed`; changing `KEY_PASS` is not currently supported.

Run exactly one daemon per Platform account. Multiple wallets can be derived and used for signing from the master key.

## Configuration

The daemon reads configuration from environment variables or a local `.env` file.

| Variable | Required | Description |
|----------|----------|-------------|
| `KEY_PASS` | Yes | Password used to encrypt and decrypt `wallet.seed`. Use a unique, high-entropy value. |
| `PLATFORM_KEY` | Yes | Enjin Platform API token used by the daemon. |
| `SEED_PATH` | No | Optional path to a seed file or seed directory. |
| `PLATFORM_URL` | No | GraphQL daemon endpoint. Defaults to `https://platform.enjin.io/graphql/daemon`. |
| `PUSHER_APP_KEY` | No | Pusher application key. Defaults to Enjin Platform's published key. |
| `PUSHER_CLUSTER` | No | Pusher cluster. Defaults to `us2`. |

Minimal `.env`:

```bash
KEY_PASS=your-unique-key-password
PLATFORM_KEY=your-platform-api-token
```

### AWS CloudFormation lifecycle

Updating `PlatformApiToken` changes its Secrets Manager value, but an already-running ECS task does not reread secrets. After the stack update completes, run the command exposed by the stack's `PlatformApiTokenRestartCommand` output to force a replacement task.

The generated EFS file system and `KEY_PASS` secret are retained when the stack is deleted. To recreate the stack with the same wallet identity, pass the previous `WalletSeedFileSystemId` and `KeyPassSecretArn` outputs as `ExistingWalletSeedFileSystemId` and `ExistingKeyPassSecretArn`. Supply both values together; leaving both empty intentionally creates a new wallet.

> **Every later `update-stack` on a recovery-mode stack must re-supply both `Existing*` values.** CloudFormation applies a parameter's default to any parameter omitted from an `update-stack` call, and the default for both is empty. Omitting them — for example in a CI job that only rolls the image — reverts the stack to "create a new wallet": a new empty EFS file system and a new `KEY_PASS` are created and the daemon starts signing from a different address. The previous wallet is not destroyed (both resources are retained and their ids remain in the old stack's outputs), but nothing fails loudly. Always pass `ParameterKey=ExistingWalletSeedFileSystemId,UsePreviousValue=true` and the same for `ExistingKeyPassSecretArn`, or use the console update flow, which pre-populates existing values. CloudFormation cannot express "this parameter was previously non-empty", so there is no template-level guard for this — the daemon does log a warning when it generates a new wallet identity, so watch for it after any stack update.

Recovery also requires the previous stack's EFS mount targets to be gone: a file system can only have mount targets in one VPC, and the new stack creates its own VPC. Delete the old stack (or its mount targets) before creating the replacement; a side-by-side rehearsal fails with `MountTargetConflict` and rolls back.

### Windows / PowerShell

The `.env` file must be UTF-8. Windows PowerShell 5.1 writes UTF-16 by default
(`>`, `Out-File`, `Set-Content` without `-Encoding`), which can corrupt `.env`.
The daemon auto-detects and reads UTF-16 `.env` files, but to be safe either
save explicitly as UTF-8:

```powershell
Set-Content -Path .env -Encoding utf8 -Value "KEY_PASS=...`nPLATFORM_KEY=..."
```

or set the variables in the session instead of using a file:

```powershell
$env:KEY_PASS="your-unique-key-password"
$env:PLATFORM_KEY="your-platform-api-token"
```

Also make sure the file is named exactly `.env`, not `.env.txt` (Notepad can add
a hidden `.txt` extension).

## Running Locally

Build and run from source:

```bash
cargo build --release
./target/release/wallet-daemon
```

Import an existing 12-word mnemonic:

```bash
./target/release/wallet-daemon import
```

Print the decrypted seed for migration:

```bash
./target/release/wallet-daemon print-seed
```

Only print the seed from a trusted shell.

## Docker

The published image is available on [Docker Hub](https://hub.docker.com/r/enjin/wallet-daemon).

## Tests

```bash
cargo test
```

This runs the unit tests plus an end-to-end suite (`tests/daemon_end_to_end.rs`)
that starts the real daemon binary against a mock Enjin Platform
(`tests/support/mod.rs`) on loopback. The daemon obtains chain metadata from the
platform via `GetChainInfo`, so the mock can drive the full fetch/sign/submit
path with no chain, no funds and no external network — including the failure
paths a live platform will not reproduce on demand: multi-page scans, rows that
cannot be signed, a platform outage, and a server that repeats its cursor.

The outage test deliberately measures elapsed time to prove the retry backoff
escalates rather than becoming a request storm, so the suite takes ~25 seconds.
To run only the fast unit tests:

```bash
cargo test --bins
```

## License

The LGPL 3.0 License. See [LICENSE](LICENSE).
