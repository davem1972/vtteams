//! Viewport-driven image queueing, download slots and cache eviction.

use crate::render::{ImageStore, ImgState};
use std::collections::{HashSet, VecDeque};

const MAX_IMAGE_JOBS: usize = 4;
const MAX_CACHED_IMAGES: usize = 32;

#[derive(Default)]
pub(super) struct Images {
    store: ImageStore,
    queue: VecDeque<String>,
    active: usize,
}

impl Images {
    pub(super) fn store(&self) -> &ImageStore {
        &self.store
    }

    pub(super) fn is_downloading(&self) -> bool {
        self.active > 0
    }

    pub(super) fn is_busy(&self) -> bool {
        self.is_downloading()
            || !self.queue.is_empty()
            || self
                .store
                .map
                .values()
                .any(|s| matches!(s, ImgState::Loading))
    }

    /// Queue accepted images near the viewport once and protect them from eviction, even with art turned off.
    /// Unverified images that are accepted (explicitly loaded) will be re-queued.
    pub(super) fn observe_viewport(
        &mut self,
        nearby: &[String],
        enabled: bool,
        accepts: impl Fn(&str) -> bool,
    ) {
        if enabled {
            for url in nearby {
                if !accepts(url) {
                    continue;
                }
                // Queue if: not in store, or is unverified (needs retry), and not already queued
                let should_queue = matches!(
                    self.store.map.get(url),
                    None | Some(ImgState::Unverified(_))
                );
                if should_queue && !self.queue.contains(url) {
                    self.queue.push_back(url.clone());
                }
            }
        }
        let protect: HashSet<&str> = nearby.iter().map(String::as_str).collect();
        // A picture queued and then scrolled well past is not worth a download slot any more; it is queued again if it
        // comes back near.
        self.queue.retain(|url| protect.contains(url.as_str()));
        self.store.evict(MAX_CACHED_IMAGES, &protect);
    }

    /// Let pictures that failed be tried again (after signing in again, or when the person asks for a refresh). Returns
    /// how many were forgotten.
    pub(super) fn retry_failed(&mut self) -> usize {
        self.store.forget_failed()
    }

    /// Claim a slot and mark the image loading together, before the caller dispatches the request.
    pub(super) fn next_download(&mut self) -> Option<String> {
        if self.active >= MAX_IMAGE_JOBS {
            return None;
        }
        let url = self.queue.pop_front()?;
        self.active += 1;
        self.store.begin(&url);
        Some(url)
    }

    pub(super) fn complete(&mut self, url: &str, state: ImgState) {
        self.active = self.active.saturating_sub(1);
        self.store.set(url, state);
    }

    #[cfg(test)]
    pub(super) fn queued(&self) -> &VecDeque<String> {
        &self.queue
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::RgbImage;
    use std::sync::Arc;

    fn ready() -> ImgState {
        ImgState::Ready(Arc::new(RgbImage::new(2, 2)))
    }

    #[test]
    fn viewport_images_are_checked_deduplicated_and_not_retried_after_failure() {
        let mut images = Images::default();
        let nearby = vec![
            "service-image".into(),
            "foreign-image".into(),
            "service-image".into(),
        ];
        let accepts = |url: &str| url.starts_with("service-");
        images.observe_viewport(&nearby, false, accepts);
        assert!(!images.is_busy());
        images.observe_viewport(&nearby, true, accepts);
        images.observe_viewport(&nearby, true, accepts);
        assert_eq!(images.next_download().as_deref(), Some("service-image"));
        images.observe_viewport(&nearby, true, accepts);
        assert!(
            images.next_download().is_none(),
            "queued and loading URLs are not duplicated"
        );
        images.complete("service-image", ImgState::Failed("unreadable".into()));
        images.observe_viewport(&nearby, true, accepts);
        assert!(images.next_download().is_none());
        assert!(!images.is_busy());
    }

    #[test]
    fn a_queued_picture_that_is_no_longer_near_the_screen_leaves_the_queue() {
        let mut images = Images::default();
        let first: Vec<String> = (0..8).map(|n| format!("image{n}")).collect();
        images.observe_viewport(&first, true, |_| true);
        assert_eq!(images.queued().len(), 8);
        // Scrolled far away: only the new neighbourhood is wanted.
        images.observe_viewport(&["image7".into(), "image9".into()], true, |_| true);
        let queued: Vec<&str> = images.queued().iter().map(String::as_str).collect();
        assert_eq!(queued, ["image7", "image9"]);
    }

    #[test]
    fn failed_pictures_are_tried_again_only_when_asked() {
        let mut images = Images::default();
        let nearby = vec!["a".to_string(), "b".to_string()];
        images.observe_viewport(&nearby, true, |_| true);
        images.next_download();
        images.next_download();
        images.complete("a", ImgState::Failed("401".into()));
        images.complete("b", ready());
        images.observe_viewport(&nearby, true, |_| true);
        assert!(images.next_download().is_none(), "not on its own");
        assert_eq!(images.retry_failed(), 1);
        images.observe_viewport(&nearby, true, |_| true);
        assert_eq!(
            images.next_download().as_deref(),
            Some("a"),
            "the failed one is fetched again; the good one is kept"
        );
        assert!(matches!(
            images.store().map.get("b"),
            Some(ImgState::Ready(_))
        ));
    }

    #[test]
    fn success_and_failure_both_release_download_slots() {
        let mut images = Images::default();
        let nearby: Vec<_> = (0..6).map(|n| format!("image{n}")).collect();
        images.observe_viewport(&nearby, true, |_| true);
        for url in &nearby[..MAX_IMAGE_JOBS] {
            assert_eq!(images.next_download().as_ref(), Some(url));
        }
        assert!(images.next_download().is_none());
        images.complete(&nearby[0], ready());
        images.complete(&nearby[1], ImgState::Failed("failed".into()));
        assert_eq!(images.next_download().as_ref(), Some(&nearby[4]));
        assert_eq!(images.next_download().as_ref(), Some(&nearby[5]));
        assert!(images.next_download().is_none());
        for url in &nearby[2..] {
            images.complete(url, ready());
        }
        assert!(!images.is_busy());
    }

    #[test]
    fn eviction_keeps_visible_and_loading_images_when_art_is_disabled() {
        let mut images = Images::default();
        let nearby: Vec<_> = (0..MAX_CACHED_IMAGES + 4)
            .map(|n| format!("image{n}"))
            .collect();
        images.observe_viewport(&nearby, true, |_| true);
        while let Some(url) = images.next_download() {
            images.complete(&url, ready());
        }
        images.observe_viewport(&["image0".into(), "loading".into()], true, |_| true);
        assert_eq!(images.next_download().as_deref(), Some("loading"));
        images.observe_viewport(&["image0".into()], false, |_| true);
        assert!(matches!(
            images.store().map.get("image0"),
            Some(ImgState::Ready(_))
        ));
        assert!(matches!(
            images.store().map.get("loading"),
            Some(ImgState::Loading)
        ));
        assert_eq!(images.store().map.len(), MAX_CACHED_IMAGES + 1);
        assert!(images.is_busy());
    }
}
