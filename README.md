# Enjin Wallet Daemon

A daemon for automatically signing transactions submitted to the Enjin Platform.

[![License: LGPL 3.0](https://img.shields.io/badge/license-LGPL_3.0-purple)](https://opensource.org/license/lgpl-3-0/)

## Functionality

The wallet will poll every 6 seconds for new transactions.

Each transaction will be signed and submitted to a node. If the transaction fails the wallet retries using exponential backoff a couple of times until it's dropped.

The wallet keeps track of the nonce locally so more than 1 transaction can be submitted by block.

## Storage for keys

Only local storage for keys is supported, the BIP-39 encoding of the key is supported and password is required.

The path to the key storage is defined in the configuration file as `master_key`. If the storage doesn't exist it's created otherwise it's read.

The password is shared through the `KEY_PASS` env variable.

Should the password be wrong the wallet will panic.

## Configuration

* By default it will use any `config.*` file supported by the crate [`config`](https://crates.io/crates/config) the file can be overriden with the env variable `CONFIG_FILE`.
* The config file should have the following fields:
  * `node`: URI for the node to connect.
  * `api`: The URI for the platform API.
  * `master_key`: path to where the key is or will be stored.
* The key that will sign the transactions will be created and stored the first time the program is run.
* If the key already exists it will be loaded.
* The API Key for the platform should be stored in the `PLATFORM_KEY` env var.

## Compile the wallet

Simply run:

`cargo build --release`

If you are going to run it without a relay chain, for testing purposes, use this instead (normally used for local integration tests):

`cargo build --features local`

Note that right now the platform only correctly encodes transactions for the matrix chain not for the relay chain so it won't be used.

## Architecture

* The `lib` directory is where most of the interesting code lives.
* In `bin` you will only find the startup code and is where the binary is created.

Externally the main things in `lib` are the "jobs" (in the aptly named module `jobs`) which are tasks run in the background:
* `PollJob` which continually polls the platform and sends the responses to the other job.
* `SignProccesor` which signs the recieved transaction requests it's sent and gives the txHash back to the platform.

You will also need to load the configuration to pass to the jobs with the `load_config` in `config_loader` module.

From inside `lib`, other than the `job` and `config_loader`, the important modules are:
* `wallet_trait` which contains multiple traits for defining a wallet (which in theory could work with any chain) and an implementation specific for the enjin blockchain.
* `chain_connection` traits related to a long-lived connection to a blockchain to submit transactions (which, again, in theory could work with any blockchain) and an implementation specific for any substrate-based chain.
* `connection_handler` contains a few wrappers to make it easier to work with long-lived connections, things like handling reconnection and re-submissions.

The `bin` uses the jobs and the configuration loading to execute the following:
1. Load the configuration, including the key, which it creates if it doesn't exist.
1. Start off the jobs which then:
1. Continously polls the platform for new transactions, the polled transactions are mutated to `PROCCESING`.
1. Sign a transaction request when it is recieved.
1. Send the signed transaction to the blockchain.
1. The recieved transaction hash is submitted to the platform changing the state of the transaction to `BROADCAST`.
1. When `ctrl-c` is pressed stop the program.

## Deployment

For deployment there is a docker image in the `docker` directory that's built using the `build.sh` script.

The docker image is tagged `enjin/wallet-daemon`.

The image start command is the script in `data/start.sh`.

It requires a volume mounted in `/opt/app/config.json` with the `config.json` file (with the details corresponding to the deployment).

Furthemore, the `config.json` requires the `master_key` entry to be `/opt/app/storage/` (For an example look at the `config.json` in the `docker` directory).

Also it expects the following secrets to be passed in the order that they are listed here:
You will need to generate a key locally by running the daemon with `cargo run --release` (with the `KEY_PASS` env variable set to something safe), to see an example of the generated key take a look at the `store` directory and use that key to set these secrets.
* `STORE_NAME`: the storage name, this is a hex number which is the file name where the key is stored, it's generated when the key is generate (In the current example: `73723235301cb3057d43941d5f631613aa1661be0354d39e34f23d4ef527396b10d2bb7a`).
* `SEED_PHRASE`: These are the content of the key file, which are the bip-39 words used to generate the key. (In the current example: "duty captain man fantasy angry window release hammer suspect bullet panda special").
* `KEY_PASS`: The pass of the key which when originally generated is set through the `KEY_PASS` env variable. (In the current example: `example`).
* `PLATFORM_KEY`: The platform key is the API token used to authenticate the wallet daemon so it can request new transactions from the platform to sign.

Another important thing to note when the seed phrase is written to the file, there is something weird that can happen with `printf` so if there is an error reading the key when deploying try defining the env variable `ADD_QUOTES` (or undefining it if it exists), if you want to know more about this read the comment in the `start.sh` script.

**REMEMBER NOT TO USE ANY OF THE KEYS IN THIS REPOSITORY FOR AN ACTUAL DEPLOYMENT**

## License

The LGPL 3.0 License. Please see [License File](LICENSE) for more information.
