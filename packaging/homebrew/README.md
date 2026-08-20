# Homebrew formula

The formula is maintained in the Haider repository itself; there is no separate
tap repository. Install it directly from the raw formula URL:

```sh
brew install --formula https://raw.githubusercontent.com/Rizzist/haider-agent/main/packaging/homebrew/haider.rb
```

The formula installs `haider` and its required sibling `haiderd` together. On
Linux it also installs `haider-wayland-portal`. macOS assets are Developer ID
signed and Apple-notarized; Linux assets are currently unsigned and are verified
with their release SHA-256 values.
