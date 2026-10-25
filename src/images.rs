//! In-memory cache of decoded images, shared by every widget that shows one.
//!
//! Downloads happen on the backend thread (`Command::FetchImage`); decoding
//! and all callbacks happen on the GTK main thread. Widgets ask with `get`
//! and are called back once, either straight away (cache hit) or when the
//! bytes arrive. Several widgets asking for the same URL cost one download.
//!
//! IMPORTANT: widgets in a recycled `ListView` row can be rebound to another
//! URL before the callback fires. Callers must check that the widget still
//! wants the URL they asked for (see `ui::load_into`).

use std::{
    cell::RefCell,
    collections::{HashMap, HashSet, VecDeque},
};

use async_channel::Sender;
use gtk::{gdk, glib};

use crate::model::Command;

/// Decoded textures kept. Avatars and resized previews are small, so this
/// bounds memory to a few tens of megabytes however long the app runs.
const MAX_CACHED: usize = 400;

type Waiter = Box<dyn FnOnce(Option<gdk::Texture>)>;

#[derive(Default)]
pub struct Images {
    textures: RefCell<HashMap<String, gdk::Texture>>,
    /// Oldest first; the front is evicted when `textures` is full.
    order: RefCell<VecDeque<String>>,
    waiting: RefCell<HashMap<String, Vec<Waiter>>>,
    /// URLs that failed to download or decode. Not retried this session, or a
    /// dead link would be refetched every time its row scrolls into view.
    failed: RefCell<HashSet<String>>,
    sender: RefCell<Option<Sender<Command>>>,
}

impl Images {
    /// Connects the cache to a backend. Pending requests of an earlier
    /// session are forgotten: their answers will never come.
    pub fn set_sender(&self, sender: Option<Sender<Command>>) {
        self.sender.replace(sender);
        self.waiting.borrow_mut().clear();
    }

    /// Calls `done` with the image, or `None` if it cannot be had.
    pub fn get(&self, url: &str, done: impl FnOnce(Option<gdk::Texture>) + 'static) {
        if let Some(texture) = self.textures.borrow().get(url) {
            return done(Some(texture.clone()));
        }
        if self.failed.borrow().contains(url) {
            return done(None);
        }
        let first = {
            let mut waiting = self.waiting.borrow_mut();
            let list = waiting.entry(url.to_owned()).or_default();
            list.push(Box::new(done));
            list.len() == 1
        };
        if first {
            if let Some(sender) = self.sender.borrow().as_ref() {
                let _ = sender.try_send(Command::FetchImage(url.to_owned()));
            }
        }
    }

    pub fn deliver(&self, url: String, bytes: Vec<u8>) {
        match gdk::Texture::from_bytes(&glib::Bytes::from_owned(bytes)) {
            Ok(texture) => {
                self.insert(url.clone(), texture.clone());
                self.finish(&url, Some(texture));
            }
            Err(_) => self.fail(url),
        }
    }

    pub fn fail(&self, url: String) {
        self.failed.borrow_mut().insert(url.clone());
        self.finish(&url, None);
    }

    fn insert(&self, url: String, texture: gdk::Texture) {
        let mut textures = self.textures.borrow_mut();
        let mut order = self.order.borrow_mut();
        if textures.insert(url.clone(), texture).is_none() {
            order.push_back(url);
        }
        while order.len() > MAX_CACHED {
            if let Some(oldest) = order.pop_front() {
                textures.remove(&oldest);
            }
        }
    }

    fn finish(&self, url: &str, texture: Option<gdk::Texture>) {
        let waiters = self.waiting.borrow_mut().remove(url).unwrap_or_default();
        for waiter in waiters {
            waiter(texture.clone());
        }
    }
}
