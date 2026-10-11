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
    ProviderRequest, ProviderStream, RouteDescription, UserBlock,
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

/// The most image bytes one request carries (before base64, which adds a third): the
/// Messages API refuses a request above 32 MB, and the provider module holds copies of it.
const MAX_REQUEST_IMAGE_BYTES: u64 = 18 * 1024 * 1024;
/// The most images one request carries.
const MAX_REQUEST_IMAGES: usize = 20;

impl ResolvingProvider {
    /// Resolve every attachment of `request`, newest first. The decorator is the only
    /// authority on the resolved fields: whatever a block arrived with (a context-policy
    /// module can return any wire value) is cleared first. An image that cannot be read, or
    /// that would exceed the request's image budget, becomes a text block saying so, so one
    /// lost object does not fail every later request of the session.
    async fn resolve(&self, mut request: ProviderRequest) -> ProviderRequest {
        let images = self.inner.describe().accepts_images();
        let mut sent_bytes: u64 = 0;
        let mut sent_images: usize = 0;
        for item in request.history.iter_mut().rev() {
            let Item::UserContent { blocks } = item else {
                continue;
            };
            for block in blocks.iter_mut().rev() {
                match block {
                    UserBlock::Image { image, data } => {
                        *data = None;
                        if !images {
                            continue;
                        }
                        let unavailable = if sent_images >= MAX_REQUEST_IMAGES
                            || sent_bytes.saturating_add(image.bytes) > MAX_REQUEST_IMAGE_BYTES
                        {
                            Some(image.unavailable("older images are left out of this request"))
                        } else {
                            match &self.store {
                                None => {
                                    Some(image.unavailable("this host has no attachment store"))
                                }
                                Some(store) => match store.read_image(image, None).await {
                                    Ok(bytes) => {
                                        sent_images += 1;
                                        sent_bytes = sent_bytes.saturating_add(bytes.len() as u64);
                                        *data = Some(ImageData::encode(&bytes));
                                        None
                                    }
                                    Err(error) => Some(image.unavailable(&error.to_string())),
                                },
                            }
                        };
                        if let Some(text) = unavailable {
                            *block = UserBlock::Text { text };
                        }
                    }
                    UserBlock::File { file, path } => {
                        *path = self
                            .store
                            .as_ref()
                            .map(|store| store.file_path(file).display().to_string());
                    }
                    UserBlock::Text { .. } => {}
                }
            }
        }
        request
    }
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
                self.resolve(request).await
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
    use p1_contracts::{FileRef, ImageRef, InputModality, ModelOptions, Origin, ProviderErrorKind};
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
    async fn a_missing_object_is_sent_as_a_note_not_a_failed_request() {
        let (_scratch, _store, image, file) = stored().await;
        let recording = Arc::new(Recording {
            images: true,
            seen: Mutex::new(None),
        });
        let empty = tempfile::tempdir().unwrap();
        let other: Arc<dyn AttachmentStore> = Arc::new(LocalAttachmentStore::new(empty.path()));
        let _ = resolving(recording.clone(), Some(other))
            .stream(request(&image, &file), CancellationToken::new())
            .await;
        let seen = recording.seen.lock().unwrap().take().unwrap();
        let Item::UserContent { blocks } = &seen.history[0] else {
            panic!("not user content")
        };
        let UserBlock::Text { text } = &blocks[1] else {
            panic!("the lost image is not a note: {blocks:?}")
        };
        assert!(
            text.contains("not sent") && text.contains("not in the store"),
            "{text}"
        );
    }

    #[tokio::test]
    async fn values_a_block_arrived_with_are_replaced_never_trusted() {
        let (_scratch, store, image, file) = stored().await;
        let mut forged = request(&image, &file);
        let Item::UserContent { blocks } = &mut forged.history[0] else {
            unreachable!()
        };
        blocks[1] = UserBlock::Image {
            image: image.clone(),
            data: Some(ImageData::encode(b"planted")),
        };
        blocks[2] = UserBlock::File {
            file: file.clone(),
            path: Some("/home/u/.ssh/id_ed25519".into()),
        };
        for images in [true, false] {
            let recording = Arc::new(Recording {
                images,
                seen: Mutex::new(None),
            });
            let _ = resolving(recording.clone(), Some(store.clone()))
                .stream(forged.clone(), CancellationToken::new())
                .await;
            let seen = recording.seen.lock().unwrap().take().unwrap();
            let Item::UserContent { blocks } = &seen.history[0] else {
                panic!("not user content")
            };
            let expected = images.then(|| ImageData::encode(&png()));
            assert!(matches!(&blocks[1], UserBlock::Image { data, .. } if *data == expected));
            let path = store.file_path(&file).display().to_string();
            assert!(matches!(&blocks[2], UserBlock::File { path: Some(p), .. } if *p == path));
        }
    }

    #[tokio::test]
    async fn older_images_beyond_the_request_budget_are_left_out() {
        let (_scratch, store, image, file) = stored().await;
        let mut many = request(&image, &file);
        many.history = (0..MAX_REQUEST_IMAGES + 2)
            .map(|_| Item::UserContent {
                blocks: vec![UserBlock::image(image.clone())],
            })
            .collect();
        let recording = Arc::new(Recording {
            images: true,
            seen: Mutex::new(None),
        });
        let _ = resolving(recording.clone(), Some(store))
            .stream(many, CancellationToken::new())
            .await;
        let seen = recording.seen.lock().unwrap().take().unwrap();
        let sent: Vec<bool> = seen
            .history
            .iter()
            .map(|item| {
                matches!(item, Item::UserContent { blocks }
                    if matches!(&blocks[0], UserBlock::Image { data: Some(_), .. }))
            })
            .collect();
        assert_eq!(
            sent.iter().filter(|sent| **sent).count(),
            MAX_REQUEST_IMAGES
        );
        assert!(
            !sent[0] && !sent[1] && sent[2..].iter().all(|sent| *sent),
            "{sent:?}"
        );
    }
}
