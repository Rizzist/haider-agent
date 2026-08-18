# CU-1: image-bearing tool results

CU-1 extends the existing artifact and attachment path; it does not add an
inline image representation to the journal.

## Reused seams

- `ArtifactRef` and the filesystem CAS remain the durable identity and byte
  store. `BoundedResult.artifact` already established the preview-plus-ref
  result pattern.
- Composer images already travel as `AttachmentBlock::Image` refs. The daemon
  resolves those refs into ephemeral `ResolvedAttachment` base64 only while
  constructing a provider request. Tool-result images use the same resolver
  and provider attachment map.
- The composer limits of five images, 5 MiB per image, and 16 MiB aggregate
  are reused for tool-result provider context.

## Additive protocol

`BoundedResult` and provider-neutral `Block::ToolResult` have an optional,
default-empty `images` array. Each entry is:

```text
ImageBlockRef {
    artifact: ArtifactRef,
    media_type: "image/png" | "image/jpeg",
    width: u32,
    height: u32,
    byte_len: u64,
}
```

The metadata describes the exact encoded CAS object. The old JSON shape still
decodes with an empty array, and an empty array is omitted when encoding, so
existing journal rows and clients retain their wire shape.

## Storage and admission

`Cas::put_image` is the single admission seam. It validates format and
dimensions before publication, admits only PNG or JPEG, and returns the
metadata ref for the bytes it actually stored. A retained image is at most
2048 pixels on either axis and 5 MiB encoded. Input is rejected before decode
above 32 MiB or 40 million pixels. PNGs are fully decoded under a 192 MiB
decoder-allocation ceiling; oversized PNG screenshots are aspect-fit
downscaled, re-encoded as PNG, and only then placed in CAS. PNG container
admission also requires valid chunk CRCs, a terminal IEND, and no trailing
payload. JPEG admission validates the SOF/SOS component grammar, quantization
and Huffman table structure, non-empty scan data, terminal EOI, and no trailing
payload. It accepts the narrow 8-bit baseline sequential Huffman JPEG subset;
extended, progressive, lossless, and arithmetic modes are rejected. Sampling factors, exactly-once
component coverage, and DRI/RST ordering are checked. It does not fully
entropy-decode JPEG pixels. A JPEG already inside
the retained bounds is admitted as JPEG; an oversized JPEG is rejected rather
than publishing an unbounded object because this build does not carry a JPEG
pixel decoder.

Tools receive this behavior through the existing `CasSink`; the production
SQLite and daemon store handles delegate to the same `Cas::put_image` path.

Before the result is journaled, the actor reads every returned ref and uses
`validate_image_block` to compare byte length, detected encoding, and actual
dimensions with its declared metadata, and recomputes the BLAKE3 address over
the supplied bytes. This is independent of provider
capability and context budgeting, so a tool cannot bypass `put_image` by
returning a generic CAS ref. The durable result contains `ImageBlockRef` only.
Admission validates all refs sequentially without retaining base64. Only the
post-budget retained suffix is resolved and cached for the ephemeral provider
request; base64 is never appended to the journal.

## Provider shaping

All native shapes preserve tool-call/result pairing and retain the text
preview:

| Provider family | Request shape |
| --- | --- |
| Anthropic Messages | Native `image` source blocks inside the matching `tool_result.content` array, after its text block. |
| Gemini | `inlineData` parts immediately after the matching `functionResponse` part in the same user content. |
| OpenAI Responses | The matching `function_call_output` item, immediately followed by a user `message` whose content contains `input_image` items. |
| OpenAI-compatible chat | The matching `role: tool` message, immediately followed by one `role: user` multimodal message containing `image_url` items. No unrelated message may be interposed. |

For an unsupported provider/model, the core explicitly replaces refs in the
provider-bound clone with honest, hard-bounded text placeholders naming each
artifact and its declared type, dimensions, and byte length. Native adapters
fail closed if an image ref reaches shaping without resolved CAS bytes. A
missing or corrupt durable CAS object fails the turn as store corruption; it
is not mislabeled as a capability fallback. The durable message is unchanged.

## Context budget

Before every provider request, the daemon/core applies one budget to the
request clone: at most five tool-result images and at most 16 MiB total encoded
bytes. If either limit is exceeded, images are removed from the chronological
front until both constraints hold. Each affected result gains a bounded note
naming its first omitted artifact and the number of additional omissions.
Durable messages and CAS objects remain unchanged.

Context-compaction requests have no image attachment channel. They first
validate every durable tool image (including refs that the budget will omit),
then apply the same oldest-first budget, then use the explicit unsupported
placeholder projection before sending the summarization request.

## Round-trip law

A tool can call `CasSink::put_image`, return the resulting `ImageBlockRef` in
`BoundedResult.images`, and have the actor preserve that ref through the
durable tool result. Immediately before the next provider call, the actor reads
the retained ref from CAS, checks `byte_len`, and adds the bytes to the
ephemeral attachment map. Provider adapters then apply the family-specific
shape above. The daemon fixture exercises this path with a real CAS and a fake
image-producing tool, without network access.
