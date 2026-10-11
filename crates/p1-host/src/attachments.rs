//! Attachments at the composition root (ADR-0180): the run's store, and the provider
//! decorator that resolves references right before a request.
//!
//! History holds references only. For one request the decorator puts an image's bytes
//! into its block when the route's model takes images (else the provider shows the
//! placeholder), and a file's read-only path into its block. Both fields are skipped by
//! serde, so nothing resolved here can reach a journal. A provider only translates.

use std::sync::Arc;

use p1_attachment_local::LocalAttachmentStore;
use p1_contracts::{
    AttachmentStore, BoxFuture, CancellationToken, ImageData, Item, Provider, ProviderError,
    ProviderErrorKind, ProviderRequest, ProviderStream, RouteDescription, UserBlock,
};

use crate::HostDeps;

/// The host's local store: `<state dir>/attachments` (ADR-0171). `None` when the host has
/// no state directory. Creates nothing until the first save.
pub(crate) fn local_store(deps: &HostDeps) -> Option<Arc<dyn AttachmentStore>> {
    let root = crate::state::root(&crate::auth::locations(deps))?;
    Some(Arc::new(
        LocalAttachmentStore::new(root.join("attachments")).with_secrets(deps.secrets.clone()),
    ))
}

/// `inner`, with every attachment reference of a request resolved through `store`.
pub(crate) fn resolving(
    inner: Arc<dyn Provider>,
    store: Option<Arc<dyn AttachmentStore>>,
) -> Arc<dyn Provider> {
    Arc::new(ResolvingProvider { inner, store })
}

struct ResolvingProvider {
    inner: Arc<dyn Provider>,
    store: Option<Arc<dyn AttachmentStore>>,
}

impl ResolvingProvider {
    async fn resolve(
        &self,
        mut request: ProviderRequest,
    ) -> Result<ProviderRequest, ProviderError> {
        let images = self.inner.describe().accepts_images();
        for item in &mut request.history {
            let Item::UserContent { blocks } = item else {
                continue;
            };
            for block in blocks {
                match block {
                    UserBlock::Image { image, data } if images && data.is_none() => {
                        let store = self.store.as_ref().ok_or_else(|| {
                            invalid(format!(
                                "image {} cannot be sent: this host has no attachment store",
                                image.id
                            ))
                        })?;
                        let bytes = store
                            .read_image(image, None)
                            .await
                            .map_err(|error| invalid(format!("image {}: {error}", image.id)))?;
                        *data = Some(ImageData::encode(&bytes));
                    }
                    UserBlock::File { file, path } if path.is_none() => {
                        *path = self
                            .store
                            .as_ref()
                            .map(|store| store.file_path(file).display().to_string());
                    }
                    _ => {}
                }
            }
        }
        Ok(request)
    }
}

fn invalid(message: String) -> ProviderError {
    ProviderError::new(ProviderErrorKind::InvalidRequest, message)
}

impl Provider for ResolvingProvider {
    fn describe(&self) -> RouteDescription {
        self.inner.describe()
    }

    /// The shape only: an unresolved image is valid here, the bytes come with `stream`.
    fn validate(&self, request: &ProviderRequest) -> Result<(), ProviderError> {
        self.inner.validate(request)
    }

    fn stream<'a>(
        &'a self,
        request: ProviderRequest,
        cancel: CancellationToken,
    ) -> BoxFuture<'a, Result<ProviderStream, ProviderError>> {
        Box::pin(async move {
            let carries_attachments = request
                .history
                .iter()
                .any(|item| matches!(item, Item::UserContent { .. }));
            let request = if carries_attachments {
                self.resolve(request).await?
            } else {
                request
            };
            self.inner.stream(request, cancel).await
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use p1_contracts::{FileRef, ImageRef, InputModality, ModelOptions, Origin};
    use std::sync::Mutex;

    /// Records the request it is asked to stream, then fails the setup.
    struct Recording {
        images: bool,
        seen: Mutex<Option<ProviderRequest>>,
    }

    impl Provider for Recording {
        fn describe(&self) -> RouteDescription {
            RouteDescription {
                origin: Origin {
                    route: "test/route".into(),
                    model: "m".into(),
                },
                supports_freeform_tools: false,
                mandatory_prompt_prefix: None,
                reports_cost: false,
                cache_key: p1_contracts::CacheKeySupport::Unsupported,
                input_modalities: if self.images {
                    vec![InputModality::Text, InputModality::Image]
                } else {
                    Vec::new()
                },
            }
        }
        fn validate(&self, _request: &ProviderRequest) -> Result<(), ProviderError> {
            Ok(())
        }
        fn stream<'a>(
            &'a self,
            request: ProviderRequest,
            _cancel: CancellationToken,
        ) -> BoxFuture<'a, Result<ProviderStream, ProviderError>> {
            *self.seen.lock().unwrap() = Some(request);
            Box::pin(async { Err(ProviderError::new(ProviderErrorKind::Transport, "recorded")) })
        }
    }

    fn png() -> Vec<u8> {
        fn chunk(kind: &[u8; 4], data: &[u8]) -> Vec<u8> {
            let mut out = (data.len() as u32).to_be_bytes().to_vec();
            out.extend_from_slice(kind);
            out.extend_from_slice(data);
            out.extend_from_slice(&[0; 4]);
            out
        }
        let mut out = b"\x89PNG\r\n\x1a\n".to_vec();
        out.extend(chunk(b"IHDR", &[0, 0, 0, 1, 0, 0, 0, 1, 8, 6, 0, 0, 0]));
        out.extend(chunk(b"IEND", &[]));
        out
    }

    async fn stored() -> (
        tempfile::TempDir,
        Arc<dyn AttachmentStore>,
        ImageRef,
        FileRef,
    ) {
        let scratch = tempfile::tempdir().unwrap();
        let store: Arc<dyn AttachmentStore> = Arc::new(LocalAttachmentStore::new(
            scratch.path().join("attachments"),
        ));
        let image = store.save_image(png(), None).await.unwrap();
        let file = store
            .save_file(b"notes".to_vec(), "notes.txt".into())
            .await
            .unwrap();
        (scratch, store, image, file)
    }

    fn request(image: &ImageRef, file: &FileRef) -> ProviderRequest {
        ProviderRequest {
            system_prompt: String::new(),
            history: vec![Item::UserContent {
                blocks: vec![
                    UserBlock::text("look"),
                    UserBlock::image(image.clone()),
                    UserBlock::file(file.clone()),
                ],
            }],
            tools: Vec::new(),
            options: ModelOptions::default(),
        }
    }

    async fn seen(images: bool) -> (Vec<UserBlock>, String) {
        let (_scratch, store, image, file) = stored().await;
        let recording = Arc::new(Recording {
            images,
            seen: Mutex::new(None),
        });
        let provider = resolving(recording.clone(), Some(store.clone()));
        let _ = provider
            .stream(request(&image, &file), CancellationToken::new())
            .await;
        let seen = recording.seen.lock().unwrap().take().unwrap();
        let Item::UserContent { blocks } = &seen.history[0] else {
            panic!("not user content")
        };
        (blocks.clone(), store.file_path(&file).display().to_string())
    }

    #[tokio::test]
    async fn an_image_route_gets_the_bytes_and_every_route_the_file_path() {
        let (blocks, path) = seen(true).await;
        let UserBlock::Image {
            data: Some(data), ..
        } = &blocks[1]
        else {
            panic!("image not resolved: {blocks:?}")
        };
        assert_eq!(data, &ImageData::encode(&png()));
        assert!(matches!(&blocks[2], UserBlock::File { path: Some(p), .. } if *p == path));
    }

    #[tokio::test]
    async fn a_text_only_route_is_not_sent_image_bytes() {
        let (blocks, path) = seen(false).await;
        assert!(matches!(&blocks[1], UserBlock::Image { data: None, .. }));
        assert!(matches!(&blocks[2], UserBlock::File { path: Some(p), .. } if *p == path));
    }

    #[tokio::test]
    async fn a_missing_object_fails_the_request_before_the_provider_runs() {
        let (_scratch, _store, image, file) = stored().await;
        let recording = Arc::new(Recording {
            images: true,
            seen: Mutex::new(None),
        });
        let empty = tempfile::tempdir().unwrap();
        let other: Arc<dyn AttachmentStore> = Arc::new(LocalAttachmentStore::new(empty.path()));
        let error = resolving(recording.clone(), Some(other))
            .stream(request(&image, &file), CancellationToken::new())
            .await
            .err()
            .unwrap();
        assert_eq!(error.kind, ProviderErrorKind::InvalidRequest);
        assert!(recording.seen.lock().unwrap().is_none());
    }
}
