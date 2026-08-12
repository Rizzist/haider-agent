#![allow(clippy::expect_used)]

use super::*;
use async_trait::async_trait;
use base64::Engine as _;
use haider_core::ArtifactReader;
use haider_protocol::ids::ArtifactRef;
use haider_protocol::provider::Block;
use haider_protocol::tool::{AttachmentBlock, PdfDeliveryMode};

struct PdfArtifact {
    artifact: ArtifactRef,
    bytes: Vec<u8>,
}

#[async_trait]
impl ArtifactReader for PdfArtifact {
    async fn read_artifact(&self, artifact: &ArtifactRef) -> Result<Vec<u8>, HaiderError> {
        if artifact == &self.artifact {
            Ok(self.bytes.clone())
        } else {
            Err(HaiderError::new(
                ErrorCode::InvalidArgument,
                "missing test artifact",
                false,
            ))
        }
    }
}

fn pdf_fixture(content: &str) -> Vec<u8> {
    format!(
        "%PDF-1.4\n\
         1 0 obj\n<< /Type /Catalog /Pages 2 0 R >>\nendobj\n\
         2 0 obj\n<< /Type /Pages /Count 1 /Kids [3 0 R] >>\nendobj\n\
         3 0 obj\n<< /Type /Page /Parent 2 0 R /Contents 4 0 R >>\nendobj\n\
         4 0 obj\n<< /Length {} >>\nstream\n{}\nendstream\nendobj\n\
         trailer\n<< /Root 1 0 R >>\n%%EOF\n",
        content.len() + 1,
        content
    )
    .into_bytes()
}

fn pdf_message(artifact: ArtifactRef, delivery: PdfDeliveryMode) -> Message {
    Message {
        role: haider_provider::MessageRole::User,
        blocks: vec![Block::Attachment(AttachmentBlock::Pdf {
            artifact,
            name: "report.pdf".into(),
            pages: 1,
            delivery,
        })],
    }
}

#[tokio::test]
async fn capability_split_keeps_native_pdf_bytes_and_extracts_elsewhere() {
    let artifact = ArtifactRef::new("blake3:pdf-split");
    let bytes = pdf_fixture("BT (daemon extracted text) Tj ET");
    let store = PdfArtifact {
        artifact: artifact.clone(),
        bytes: bytes.clone(),
    };

    let mut native = vec![pdf_message(
        artifact.clone(),
        PdfDeliveryMode::NativeDocument,
    )];
    let resolved = resolve_prompt_attachments(&store, &mut native, FeatureResolve::Native)
        .await
        .expect("native shaping");
    assert!(matches!(
        native[0].blocks.as_slice(),
        [Block::Attachment(AttachmentBlock::Pdf { .. })]
    ));
    assert_eq!(resolved.len(), 1);
    assert_eq!(
        base64::engine::general_purpose::STANDARD
            .decode(&resolved[0].data_base64)
            .expect("base64"),
        bytes
    );

    let mut emulated = vec![pdf_message(artifact, PdfDeliveryMode::ExtractedText)];
    let resolved =
        resolve_prompt_attachments(&store, &mut emulated, FeatureResolve::ExplicitlyEmulated)
            .await
            .expect("text shaping");
    assert!(resolved.is_empty(), "extracted PDFs do not ship base64");
    let Block::Text { text } = &emulated[0].blocks[0] else {
        panic!("fallback must become provider-neutral text");
    };
    assert!(text.starts_with("<file name=\"report.pdf\" pages=\"1\" source=\"pdf\">"));
    assert!(text.contains("daemon extracted text"));
}

#[tokio::test]
async fn image_only_pdf_is_typed_for_extraction_but_valid_natively() {
    let artifact = ArtifactRef::new("blake3:pdf-image-only");
    let store = PdfArtifact {
        artifact: artifact.clone(),
        bytes: pdf_fixture("q 100 0 0 100 0 0 cm /Im0 Do Q"),
    };
    let mut emulated = vec![pdf_message(
        artifact.clone(),
        PdfDeliveryMode::ExtractedText,
    )];
    let error =
        resolve_prompt_attachments(&store, &mut emulated, FeatureResolve::ExplicitlyEmulated)
            .await
            .expect_err("image-only extraction fails");
    let presentation = error.presentation.expect("typed presentation");
    assert_eq!(presentation.subcode.as_str(), "pdf-no-extractable-text");
    assert!(presentation.detail.contains("scanned images"));

    let mut native = vec![pdf_message(artifact, PdfDeliveryMode::NativeDocument)];
    let resolved = resolve_prompt_attachments(&store, &mut native, FeatureResolve::Native)
        .await
        .expect("native image-only PDF remains valid");
    assert_eq!(resolved.len(), 1);
}

/// The capability→delivery join, pinned both ways. An inverted join survived
/// the capability-table and shaping laws (each end was pinned, the join was
/// not); this law observes the decision itself.
///
/// MUTATION CHECK: invert the `== FeatureResolve::Native` comparison in
/// `pdf_delivery_for_provider`. Expected failure: every row below flips.
#[test]
fn pdf_delivery_join_maps_capability_to_mode() {
    use crate::session_hub::pdf_delivery_for_provider;
    for provider in ["anthropic", "anthropic-oauth", "bedrock", "vertex"] {
        assert_eq!(
            pdf_delivery_for_provider(provider),
            PdfDeliveryMode::NativeDocument,
            "{provider} is native-capable"
        );
    }
    for provider in [
        "openai",
        "openai-oauth",
        "deepseek",
        "kimi",
        "custom-profile",
    ] {
        assert_eq!(
            pdf_delivery_for_provider(provider),
            PdfDeliveryMode::ExtractedText,
            "{provider} must fall back to extraction"
        );
    }
}
