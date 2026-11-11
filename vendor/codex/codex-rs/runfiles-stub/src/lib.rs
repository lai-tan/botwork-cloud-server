use std::path::PathBuf;

pub struct Runfiles;

impl Runfiles {
    pub fn create() -> Result<Self, RunfilesError> {
        Err(RunfilesError("runfiles not available".into()))
    }

    pub fn rlocation(&self, _path: &str) -> Option<PathBuf> {
        None
    }
}

#[derive(Debug)]
pub struct RunfilesError(String);

impl std::fmt::Display for RunfilesError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl std::error::Error for RunfilesError {}

#[macro_export]
macro_rules! rlocation {
    ($rf:expr, $path:expr) => {
        $rf.rlocation($path)
    };
}