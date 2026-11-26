//! Opening a link or a saved picture in the user's browser, behind a trait so tests never launch one.

/// Something that can show a URL to the user.
pub trait Opener: Send + Sync + std::fmt::Debug {
    /// Open `url` (a web link or a `file://` URL).
    ///
    /// # Errors
    /// A message describing why nothing could be opened.
    fn open(&self, url: &str) -> Result<(), String>;
}

/// The default browser.
#[derive(Debug, Clone, Copy)]
pub struct Browser;

impl Opener for Browser {
    fn open(&self, url: &str) -> Result<(), String> {
        webbrowser::open(url).map_err(|e| e.to_string())
    }
}

/// Remembers what it was asked to open, for tests.
#[cfg(test)]
#[derive(Debug, Default)]
pub struct Recorder(std::sync::Mutex<Vec<String>>);

#[cfg(test)]
impl Recorder {
    /// Everything opened so far.
    pub fn opened(&self) -> Vec<String> {
        self.0.lock().unwrap().clone()
    }
}

#[cfg(test)]
impl Opener for Recorder {
    fn open(&self, url: &str) -> Result<(), String> {
        self.0.lock().unwrap().push(url.to_string());
        Ok(())
    }
}
