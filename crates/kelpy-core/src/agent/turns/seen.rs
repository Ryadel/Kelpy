//! The images of the administrator's messages, kept in memory for the model while Kelpy runs. Conversations store
//! their text alone: once an image is dropped, here to make room or when Kelpy restarts, the model reads its message
//! as stored, with the text read from the image.

use std::collections::{BTreeMap, HashMap};

use kelpy_providers::Images;

/// The most the images kept may take, encoded.
const MAX_KEPT: usize = 128 << 20;

/// The images kept, by message: the oldest messages' go first.
#[derive(Default)]
pub(super) struct Seen {
    messages: BTreeMap<i64, Shown>,
    size: usize,
}

struct Shown {
    session_id: i64,
    images: Images,
    /// The tokens they take.
    tokens: u64,
    size: usize,
}

impl Seen {
    /// Keeps `images`, taking `tokens`, for the message `message_id` of the conversation `session_id`, dropping
    /// the oldest messages' images to keep within [`MAX_KEPT`].
    pub(super) fn keep(&mut self, session_id: i64, message_id: i64, images: Images, tokens: u64) {
        let size = images.images.iter().map(|image| image.data.len()).sum();
        if size > MAX_KEPT {
            return;
        }
        while self.size + size > MAX_KEPT {
            let Some((_, dropped)) = self.messages.pop_first() else {
                break;
            };
            self.size -= dropped.size;
        }
        self.size += size;
        self.messages.insert(
            message_id,
            Shown {
                session_id,
                images,
                tokens,
                size,
            },
        );
    }

    /// The images kept for the messages of the conversation `session_id`, the latest messages' first, as many as
    /// take at most `limit` tokens; and the tokens they take. The others' messages go as text alone.
    pub(super) fn of(&self, session_id: i64, limit: u64) -> (HashMap<i64, Images>, u64) {
        let mut tokens = 0;
        let images = self
            .messages
            .iter()
            .rev()
            .filter(|(_, shown)| shown.session_id == session_id)
            .take_while(|(_, shown)| {
                tokens += shown.tokens;
                tokens <= limit
            })
            .map(|(&message_id, shown)| (message_id, shown.images.clone()))
            .collect::<HashMap<_, _>>();
        let tokens = images
            .keys()
            .map(|message_id| self.messages[message_id].tokens)
            .sum();
        (images, tokens)
    }

    /// Whether images are kept for the conversation `session_id`.
    pub(super) fn has(&self, session_id: i64) -> bool {
        self.messages
            .values()
            .any(|shown| shown.session_id == session_id)
    }

    /// Drops the images of the conversation `session_id`, deleted.
    pub(super) fn forget(&mut self, session_id: i64) {
        self.messages
            .retain(|_, shown| shown.session_id != session_id);
        self.size = self.messages.values().map(|shown| shown.size).sum();
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use kelpy_providers::Image;

    use super::*;

    fn images(size: usize) -> Images {
        Images {
            text: "Why?".into(),
            images: vec![Arc::new(Image {
                media_type: "image/png",
                data: "a".repeat(size),
            })],
        }
    }

    #[test]
    fn the_oldest_images_make_room_and_a_deleted_conversation_s_go() {
        let mut seen = Seen::default();
        seen.keep(1, 10, images(MAX_KEPT / 2), 1000);
        seen.keep(2, 11, images(MAX_KEPT / 4), 500);
        assert_eq!(seen.of(1, u64::MAX).1, 1000);
        // No room for both: the oldest goes.
        seen.keep(1, 12, images(MAX_KEPT / 2), 700);
        let (first, tokens) = seen.of(1, u64::MAX);
        assert_eq!(
            (first.keys().copied().collect::<Vec<_>>(), tokens),
            (vec![12], 700)
        );
        assert_eq!(seen.of(2, u64::MAX).1, 500);
        // Larger than all that may be kept: not kept.
        seen.keep(3, 13, images(MAX_KEPT + 1), 1);
        assert!(seen.of(3, u64::MAX).0.is_empty());

        seen.forget(1);
        assert!(seen.of(1, u64::MAX).0.is_empty());
        assert_eq!(seen.size, MAX_KEPT / 4);
    }

    #[test]
    fn the_latest_images_are_shown_within_the_limit() {
        let mut seen = Seen::default();
        for (message, tokens) in [(1, 300), (2, 500), (3, 400)] {
            seen.keep(7, message, images(10), tokens);
        }
        let shown = |limit| {
            let (images, tokens) = seen.of(7, limit);
            let mut messages: Vec<i64> = images.into_keys().collect();
            messages.sort_unstable();
            (messages, tokens)
        };
        assert_eq!(shown(1_200), (vec![1, 2, 3], 1_200));
        assert_eq!(shown(1_000), (vec![2, 3], 900));
        assert_eq!(shown(399), (vec![], 0));
    }
}
