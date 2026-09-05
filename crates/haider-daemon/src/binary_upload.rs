//! One bounded, unpublished CAS transfer per connection. Each frame is
//! acknowledged only after its blocking disk work; the connection continues
//! reading Pings while that work is pending.
use haider_core::SqliteStoreHandle;
use haider_protocol::ids::ArtifactRef;
use haider_rpc::{RequestId, ResponseBody, binary_artifact::Frame};

pub(crate) struct Upload {
    sink: haider_store::CasUpload,
    digest: ArtifactRef,
}

pub(crate) type Outcome = (Option<Upload>, RequestId, ResponseBody);

pub(crate) fn error(message: impl Into<String>) -> ResponseBody {
    ResponseBody::Error {
        code: haider_rpc::ERROR_CODE_INVALID_ARGUMENT.into(),
        message: message.into(),
        retryable: false,
        data: None,
    }
}

fn store_error(error: haider_protocol::error::HaiderError) -> ResponseBody {
    // Same CAS error mapping as HubConnection::respond_turn_error.
    ResponseBody::Error {
        code: haider_rpc::ERROR_CODE_INVALID_ARGUMENT.into(),
        message: error.message,
        retryable: error.retryable,
        data: None,
    }
}

pub(crate) async fn apply(
    frame: Frame,
    upload: Option<Upload>,
    store: SqliteStoreHandle,
) -> Outcome {
    let request_id = match &frame {
        Frame::Begin { request_id, .. }
        | Frame::Chunk { request_id, .. }
        | Frame::Finish { request_id } => request_id.clone(),
    };
    if let Frame::Begin { bytes, digest, .. } = frame {
        if upload.is_some() {
            return (None, request_id, error("binary upload already active"));
        }
        return match store.begin_cas_put(bytes).await {
            Ok(sink) => (
                Some(Upload { sink, digest }),
                request_id,
                ResponseBody::ArtifactPutProgress { bytes: 0 },
            ),
            Err(err) => (None, request_id, store_error(err)),
        };
    }
    let result = tokio::task::spawn_blocking(move || {
        let Some(mut upload) = upload else {
            return (None, error("no binary upload active"));
        };
        match frame {
            Frame::Chunk { offset, bytes, .. } => {
                if upload.sink.received_len() != offset {
                    return (None, error("binary upload offset mismatch"));
                }
                match upload.sink.write_chunk(&bytes) {
                    Ok(()) => {
                        let bytes = upload.sink.received_len();
                        (Some(upload), ResponseBody::ArtifactPutProgress { bytes })
                    }
                    Err(err) => {
                        store.note_cas_write_error(&err);
                        (None, store_error(err))
                    }
                }
            }
            Frame::Finish { .. } => {
                let bytes = upload.sink.received_len();
                match upload.sink.finish(&upload.digest) {
                    Ok(artifact) => (None, ResponseBody::ArtifactPut { artifact, bytes }),
                    Err(err) => {
                        store.note_cas_write_error(&err);
                        (None, store_error(err))
                    }
                }
            }
            Frame::Begin { .. } => (None, error("unexpected binary begin")),
        }
    })
    .await;
    match result {
        Ok((upload, response)) => (upload, request_id, response),
        Err(err) => (None, request_id, error(err.to_string())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use haider_protocol::error::{ErrorCode, HaiderError};

    #[test]
    fn binary_cas_error_preserves_json_upload_retryability() {
        for (code, retryable) in [
            (ErrorCode::StoreUnavailable, true),
            (ErrorCode::InvalidArgument, false),
        ] {
            assert!(
                matches!(store_error(HaiderError::new(code, "failure", retryable)), ResponseBody::Error { code, message, retryable: actual, .. }
                if code == haider_rpc::ERROR_CODE_INVALID_ARGUMENT && message == "failure" && actual == retryable)
            );
        }
    }
}
