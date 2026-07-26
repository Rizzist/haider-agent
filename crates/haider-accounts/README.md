# haider-accounts

Credential descriptors are stored in `accounts.json` under the profile
directory. Secret bytes are stored separately through the `Vault` interface.

`KeychainVault` uses macOS Security.framework generic-password entries with
service `ai.haider.agent`. Its integration test is marked `#[ignore]` because
Security.framework may display Keychain permission UI; headless CI must not run
that test. Run it manually on macOS with:

```sh
cargo test -p haider-accounts --test accounts_tests keychain_round_trip -- --ignored
```
