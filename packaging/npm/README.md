# haider-agent

npm distribution for the Haider `haider` coding-agent TUI.

```sh
npm install -g haider-agent
haider
```

The postinstall script downloads the platform archive from the matching GitHub
release, verifies its release-provided SHA-256 sidecar, and stores `haider` and
its required sibling daemon `haiderd` together under this package's `vendor/`
directory. Linux packages also retain `haider-wayland-portal`.

macOS release binaries are Developer ID signed and Apple-notarized. Linux and
Windows release binaries are currently unsigned; every downloaded archive is
still checked against its published SHA-256 checksum.
