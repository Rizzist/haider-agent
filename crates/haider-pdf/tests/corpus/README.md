# PDF parity corpus

These checked-in fixtures make backend parity tests independent of the host
machine and network. Mutation-derived cases are generated deterministically in
the test from these immutable inputs rather than stored as redundant binaries.

| File | Class | Source | SHA-256 |
| --- | --- | --- | --- |
| `qoi-specification.pdf` | real-world, object/xref streams | `qoi` 0.4.1 package, git `e97077e527618a07413f7895a3792a6859afac59`, MIT OR Apache-2.0 | `86a3362ad7142cb1b8002f05c77ba8b11008d5f3d8c86b13a1c14bb403cfc821` |
| `lopdf-unicode.pdf` | Unicode text | `lopdf` v0.42.0, git `2ffc4b4a1912c5cef08d1fa616e2687576f84a4e`, MIT | `af1792a7a2daf92f9df1ea6027801df0764debfada25b111c58d7e78c2395540` |
| `lopdf-incremental.pdf` | incremental updates | same `lopdf` source and license | `1c21fe1e0d74a46ece0e991afdca6f00036974fcd74ca4cde1d4a4cc63e23da2` |
| `pdfjs-encrypted.pdf` | real password-protected document | Mozilla PDF.js, git `abc6d413c572b4d71b8898d691813e53ccd83b3a`, Apache-2.0 | `0f44da5152adf8cafcd7b0057d840310d78e17d033ee5414da0881b6dcd130ab` |

The lopdf fixtures retain the upstream MIT notice: Copyright (c) 2016
Junfeng Liu. The complete license text is available in the repository root's
MIT license and at the pinned upstream revision.
