use indicatif::{ProgressBar, ProgressStyle};

pub struct ProgressTracker {
    progress_bar: ProgressBar,
}

impl ProgressTracker {
    pub fn new() -> Self {
        let progress_bar = ProgressBar::new(100);
        progress_bar.set_style(ProgressStyle::default_bar()
            .template("{spinner:.green} [{elapsed_precise}] [{bar:40.cyan/blue}] {percent}% ({eta})")
            .progress_chars("=> "));

        Self { progress_bar }
    }

    pub fn set_progress(&self, progress: u8) {
        self.progress_bar.set_position(progress as u64);
    }

    pub fn finish(&self) {
        self.progress_bar.finish();
    }
}
