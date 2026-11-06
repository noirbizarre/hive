//! Client cursors are opaque attachment-local handles, never encoded backend secrets.

use super::runtime::RuntimeError;
use std::{
    collections::HashMap,
    time::{Duration, Instant},
};

#[derive(Clone)]
pub(super) struct Page {
    pub method: String,
    pub backend: usize,
    pub cursor: Option<String>,
    pub generation: Option<String>,
    pub count: usize,
    expires: Instant,
}

impl Page {
    pub fn first(method: &str) -> Self {
        Self {
            method: method.into(),
            backend: 0,
            cursor: None,
            generation: None,
            count: 0,
            expires: Instant::now() + Duration::from_secs(60),
        }
    }
    pub fn advance(
        &mut self,
        cursor: Option<String>,
        generation: String,
    ) -> Result<(), RuntimeError> {
        self.count += 1;
        if self.count > 256
            || cursor
                .as_ref()
                .is_some_and(|c| c.len() > 8192 || Some(c) == self.cursor.as_ref())
        {
            return Err(RuntimeError::Response);
        }
        if cursor.is_some() {
            self.generation = Some(generation);
        } else {
            self.backend += 1;
            self.generation = None;
        }
        self.cursor = cursor;
        Ok(())
    }
}

#[derive(Default)]
pub(super) struct Cursors(HashMap<String, Page>);
impl Cursors {
    pub fn get(&mut self, method: &str, token: &str) -> Result<Page, RuntimeError> {
        self.prune();
        self.0
            .get(token)
            .filter(|p| p.method == method)
            .cloned()
            .ok_or(RuntimeError::Request)
    }
    pub fn insert(&mut self, page: Page) -> Result<String, RuntimeError> {
        self.prune();
        if self.0.len() >= 256 {
            return Err(RuntimeError::Busy);
        }
        let token = uuid::Uuid::new_v4().to_string();
        self.0.insert(token.clone(), page);
        Ok(token)
    }
    fn prune(&mut self) {
        self.0.retain(|_, page| page.expires > Instant::now());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn cursors_are_method_and_attachment_scoped_and_expire_without_exposing_backend_state() {
        let mut cursors = Cursors::default();
        let mut page = Page::first("tools/list");
        page.advance(Some("secret-backend-cursor".into()), "generation-1".into())
            .unwrap();
        let token = cursors.insert(page).unwrap();
        assert!(!token.contains("secret"));
        assert_eq!(
            cursors.get("tools/list", &token).unwrap().cursor.as_deref(),
            Some("secret-backend-cursor")
        );
        assert!(cursors.get("resources/list", &token).is_err());
        assert!(Cursors::default().get("tools/list", &token).is_err());
        cursors.0.get_mut(&token).unwrap().expires = Instant::now() - Duration::from_secs(1);
        assert!(cursors.get("tools/list", &token).is_err());
    }
    #[test]
    fn repeated_cursors_and_unbounded_traversals_are_rejected() {
        let mut page = Page::first("tools/list");
        page.advance(Some("cursor".into()), "generation".into())
            .unwrap();
        assert!(
            page.advance(Some("cursor".into()), "generation".into())
                .is_err()
        );
        page.count = 256;
        assert!(page.advance(None, "generation".into()).is_err());
    }
}
