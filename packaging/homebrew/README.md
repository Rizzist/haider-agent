# Homebrew formula

Install from the tap:

```sh
brew install Rizzist/tap/haidercode
```

Or tap once and use the short name — `haider` is registered as an alias, so both
of these work:

```sh
brew tap Rizzist/tap
brew install haidercode
brew install haider
```

**Homebrew 6.0 and later refuse to load formulae from an untrusted third-party
tap.** If you see `Refusing to load formula ... from untrusted tap`, run:

```sh
brew trust Rizzist/tap
```

Upgrades work normally (`brew upgrade haidercode`), which the previous raw-URL
install did not support.

The formula installs `haider` and its required sibling `haiderd` together. On
Linux it also installs `haider-wayland-portal`. macOS assets are Developer ID
signed and Apple-notarized; Linux assets are currently unsigned and are verified
with their release SHA-256 values.

## Why a tap and not homebrew-core

`brew install haider` with no tap prefix would require the formula to live in
homebrew-core, which we do not qualify for: core requires an OSI-approved
open-source licence (Haider ships under `LicenseRef-KOA-P-1.0`) and prefers
formulae that build from source rather than shipping pre-built binaries. A tap
is a plain GitHub repository, needs no approval, and supports `brew upgrade`
identically.

## Source of truth

`haider.rb` in this directory is the canonical formula. The tap repository
[Rizzist/homebrew-tap](https://github.com/Rizzist/homebrew-tap) holds a copy at
`Formula/haidercode.rb` — identical except for the class name, which Homebrew
requires to match the filename.

Both are re-pinned automatically by the `repin-packages` job in
`.github/workflows/release.yml` on every `v*` tag. Do not hand-edit `version`,
`url` or `sha256`; change the workflow instead.

The tap push requires a `TAP_TOKEN` repository secret — a PAT with
`contents:write` on `Rizzist/homebrew-tap`. Without it the release still
succeeds and the workflow emits a warning; only the tap is left on its previous
version.
