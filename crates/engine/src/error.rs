use std::path::PathBuf;

use pdfium_render::prelude::{PdfiumError, PdfiumInternalError};

#[derive(Debug, thiserror::Error)]
pub enum EngineError {
    #[error(
        "PDFium library not found (looked in: {searched}). Run scripts/fetch-pdfium first, \
         or set PDFIUM_LIB_DIR to the folder that contains it"
    )]
    LibraryNotFound { searched: String },

    #[error("failed to load PDFium from {path}: {reason}")]
    LibraryLoad { path: PathBuf, reason: String },

    #[error("this PDF is password protected")]
    PasswordRequired,

    #[error("could not open PDF: {0}")]
    Open(String),

    #[error("document is not open")]
    UnknownDocument,

    #[error("page {0} does not exist")]
    PageOutOfRange(u32),

    #[error("tile is outside the page")]
    TileOutOfRange,

    #[error("PDFium error: {0}")]
    Pdfium(String),

    #[error("the engine thread has stopped")]
    Stopped,

    /// The tile was abandoned part way because a newer [`crate::Engine::set_wanted`] no
    /// longer asked for it.
    #[error("rendering was cancelled")]
    Cancelled,

    #[error(transparent)]
    Io(#[from] std::io::Error),
}

impl EngineError {
    pub(crate) fn from_open(error: PdfiumError) -> Self {
        match error {
            PdfiumError::PdfiumLibraryInternalError(PdfiumInternalError::PasswordError) => {
                EngineError::PasswordRequired
            }
            PdfiumError::IoError(e) => EngineError::Io(e),
            other => EngineError::Open(format!("{other:?}")),
        }
    }
}

impl From<PdfiumError> for EngineError {
    fn from(error: PdfiumError) -> Self {
        EngineError::Pdfium(format!("{error:?}"))
    }
}
