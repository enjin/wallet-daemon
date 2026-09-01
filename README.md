# Enjin Wallet Daemon

A lightweight outbound-only signer for Enjin Platform transactions.

[![License: LGPL 3.0](https://img.shields.io/badge/license-LGPL_3.0-purple)](https://opensource.org/license/lgpl-3-0/)

The daemon listens for authenticated Enjin Platform WebSocket events, fetches pending work through GraphQL, signs it with the configured wallet, and returns signed payloads to the platform for broadcast. A five-minute safety poll covers missed events while the subscription is healthy; if Pusher is unavailable, the daemon automatically resumes six-second polling until the authenticated subscription is restored. It does not accept inbound transaction requests, so the host running the daemon should not expose public ports for daemon traffic.

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

## License

The LGPL 3.0 License. See [LICENSE](LICENSE).
