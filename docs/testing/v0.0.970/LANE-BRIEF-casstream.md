# Lane casstream — streamed CAS + binary artifact.put + transparent indirection (v0.0.970)
Read 969-common.md FIRST. Branch lane-970-casstream. Deliver: (1) negotiated BINARY
artifact.put (the 264 MiB case) streamed through the CAS instead of a JSON body — length-
prefixed frames on the existing wire, feature-bit gated, additive; (2) CAS transparent
indirection: large text payloads referenced by digest internally with NO exposed reference
in the client-visible schema (the exposed-reference design was REJECTED); (3) streamed
CAS read for PDFs/large blobs. Measure: peak RSS and wall for a 264 MiB put before/after
(load<10, N>=3). Pins: digest integrity, partial-frame abort, resume after disconnect
(uses 968 resume seam). docs/testing/v0.0.970/casstream.md. LAST line: SHIP/NO_SHIP.
